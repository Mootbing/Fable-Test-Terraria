//! Client mirror of server entities, keyed by id from
//! `ItemDropSpawn`/`EntitySpawn` and advanced by `EntityUpdate` snapshot
//! batches (interpolated ~100 ms in the past, like remote players).
//!
//! Item drops render as a bobbing, spinning 12 px swatch; enemies render as
//! characterful primitives per §5.1 species (squishing slimes, shambling
//! humanoids, veined demon eyes, flapping bats, glowing ash demons);
//! projectiles as velocity-rotated arrows and spinning sickles. Combat
//! feedback: white hit flash + floating damage numbers on `EntityHurt`
//! (crits larger/yellow), a health bar above damaged enemies that fades
//! 3 s after the last hit, and a death poof on `Killed` despawns.

use std::collections::{HashMap, VecDeque};

use macroquad::prelude::*;

use ferraria_shared::enemies::EnemyKind;
use ferraria_shared::items::ItemId;
use ferraria_shared::physics::hitbox;
use ferraria_shared::protocol::{DespawnReason, EntityKind, EntityState};
use ferraria_shared::tiles::TileId;
use ferraria_shared::TILE_SIZE;

use crate::light::LightEngine;
use crate::render::{item_color, lit_color, tile_color};
use crate::ui::shadow_text;

/// On snapshot gaps, extrapolate along the last velocity at most this long
/// (matches remote players).
const MAX_EXTRAPOLATION: f64 = 0.10;
/// Snapshots buffered per entity (~3 s at 20/s).
const SNAPSHOT_BUFFER: usize = 64;

/// Item drop sprite: 12 px square.
const ITEM_PX: f32 = 12.0;
const BOB_PX: f32 = 2.0;
const BOB_HZ: f32 = 1.2;
const SPIN_HZ: f32 = 0.4;

/// Hit-flash duration after `EntityHurt`.
const FLASH_SECS: f64 = 0.12;
/// Health bars over damaged enemies fade out after this long without a hit.
const HEALTH_BAR_SECS: f64 = 3.0;
const HEALTH_BAR_FADE: f64 = 0.5;
/// Floating damage numbers: rise + fade over this long.
const FLOATY_SECS: f64 = 0.9;
const FLOATY_RISE_PX: f32 = 28.0;
/// Death poof particle burst lifetime.
const POOF_SECS: f64 = 0.45;
const POOF_PARTICLES: u32 = 10;

/// What the client knows about a mirrored entity.
pub enum Kind {
    Item {
        item: ItemId,
        count: u16,
    },
    Enemy(EnemyKind),
    Projectile(EntityKind),
    /// Falling sand draws as its tile.
    Other(EntityKind),
}

struct Snap {
    t: f64,
    pos: (f32, f32),
    vel: (f32, f32),
}

struct Entity {
    kind: Kind,
    snaps: VecDeque<Snap>,
    /// Mirrored HP (enemies; synced by `EntityUpdate` hp deltas).
    hp: u16,
    max_hp: u16,
    /// Time of the last `EntityHurt` (drives flash + health-bar fade).
    hurt_t: f64,
    /// Last sampled draw position (poof origin when it dies).
    last_pos: (f32, f32),
}

impl Entity {
    /// Interpolated AABB top-left at `render_t`, pruning consumed history.
    fn sample(&mut self, render_t: f64) -> (f32, f32) {
        while self.snaps.len() >= 2 && self.snaps[1].t <= render_t {
            self.snaps.pop_front();
        }
        let Some(a) = self.snaps.front() else {
            return (0.0, 0.0);
        };
        match self.snaps.get(1) {
            Some(b) => {
                let span = b.t - a.t;
                let f = if span > 0.0 {
                    ((render_t - a.t) / span).clamp(0.0, 1.0) as f32
                } else {
                    1.0
                };
                (
                    a.pos.0 + (b.pos.0 - a.pos.0) * f,
                    a.pos.1 + (b.pos.1 - a.pos.1) * f,
                )
            }
            None => {
                let dt = (render_t - a.t).clamp(0.0, MAX_EXTRAPOLATION) as f32;
                (a.pos.0 + a.vel.0 * dt, a.pos.1 + a.vel.1 * dt)
            }
        }
    }

    /// Latest known velocity (animation: walk cycles, arrow rotation).
    fn latest_vel(&self) -> (f32, f32) {
        self.snaps.back().map(|s| s.vel).unwrap_or((0.0, 0.0))
    }

    fn push(&mut self, snap: Snap) {
        if self.snaps.len() >= SNAPSHOT_BUFFER {
            self.snaps.pop_front();
        }
        self.snaps.push_back(snap);
    }

    fn size(&self) -> (f32, f32) {
        match &self.kind {
            Kind::Item { .. } => hitbox::ITEM_DROP,
            Kind::Enemy(kind) => kind.data().size,
            Kind::Projectile(EntityKind::VoidSickleProjectile) => hitbox::VOID_SICKLE,
            Kind::Projectile(_) => hitbox::ARROW,
            Kind::Other(_) => hitbox::FALLING_TILE,
        }
    }
}

/// A floating damage number.
struct Floaty {
    text: String,
    /// World tiles at spawn.
    pos: (f32, f32),
    t0: f64,
    crit: bool,
}

/// A death-poof particle burst.
struct Poof {
    pos: (f32, f32),
    t0: f64,
    seed: u32,
    color: Color,
}

/// All mirrored entities + transient combat feedback effects.
pub struct Entities {
    map: HashMap<u32, Entity>,
    floaties: Vec<Floaty>,
    poofs: Vec<Poof>,
}

impl Entities {
    pub fn new() -> Entities {
        Entities {
            map: HashMap::new(),
            floaties: Vec::new(),
            poofs: Vec::new(),
        }
    }

    /// `ItemDropSpawn` (also used as the re-sync when a chunk subscribes:
    /// duplicates simply replace the mirror).
    pub fn spawn_item(
        &mut self,
        id: u32,
        item: ItemId,
        count: u16,
        pos: (f32, f32),
        vel: (f32, f32),
        now: f64,
    ) {
        self.spawn(id, Kind::Item { item, count }, pos, vel, now);
    }

    /// Generic `EntitySpawn` (enemies, projectiles, falling sand).
    pub fn spawn_other(
        &mut self,
        id: u32,
        kind: EntityKind,
        pos: (f32, f32),
        vel: (f32, f32),
        now: f64,
    ) {
        let kind = match EnemyKind::from_wire(kind) {
            Some(e) => Kind::Enemy(e),
            None => match kind {
                EntityKind::ArrowProjectile
                | EntityKind::FlamingArrowProjectile
                | EntityKind::VoidSickleProjectile => Kind::Projectile(kind),
                other => Kind::Other(other),
            },
        };
        self.spawn(id, kind, pos, vel, now);
    }

    fn spawn(&mut self, id: u32, kind: Kind, pos: (f32, f32), vel: (f32, f32), now: f64) {
        let mut snaps = VecDeque::new();
        snaps.push_back(Snap { t: now, pos, vel });
        let max_hp = match &kind {
            Kind::Enemy(e) => e.data().max_hp,
            _ => 0,
        };
        self.map.insert(
            id,
            Entity {
                kind,
                snaps,
                hp: max_hp,
                max_hp,
                hurt_t: f64::NEG_INFINITY,
                last_pos: pos,
            },
        );
    }

    /// One `EntityUpdate` batch (ids we never saw spawn are skipped — their
    /// chunk isn't subscribed yet).
    pub fn update(&mut self, batch: &[EntityState], now: f64) {
        for s in batch {
            if let Some(e) = self.map.get_mut(&s.id) {
                if let Some(hp) = s.hp {
                    e.hp = hp;
                }
                e.push(Snap {
                    t: now,
                    pos: s.pos,
                    vel: s.vel,
                });
            }
        }
    }

    /// `EntityHurt`: hit flash + floating damage number (crit pops).
    pub fn on_hurt(&mut self, id: u32, damage: u32, crit: bool, now: f64) {
        let Some(e) = self.map.get_mut(&id) else {
            return;
        };
        e.hurt_t = now;
        let (w, _) = e.size();
        let pos = (e.last_pos.0 + w / 2.0, e.last_pos.1 - 0.3);
        self.floaties.push(Floaty {
            text: damage.to_string(),
            pos,
            t0: now,
            crit,
        });
    }

    /// `EntityDespawn`: `Killed` plays a death poof at the last drawn spot.
    pub fn on_despawn(&mut self, id: u32, reason: DespawnReason, now: f64) {
        if let Some(e) = self.map.remove(&id) {
            if reason == DespawnReason::Killed {
                let (w, h) = e.size();
                let color = match &e.kind {
                    Kind::Enemy(kind) => enemy_body_color(*kind),
                    Kind::Projectile(EntityKind::VoidSickleProjectile) => SICKLE_BODY,
                    _ => Color::new(0.85, 0.85, 0.85, 1.0),
                };
                self.poofs.push(Poof {
                    pos: (e.last_pos.0 + w / 2.0, e.last_pos.1 + h / 2.0),
                    t0: now,
                    seed: id,
                    color,
                });
            }
        }
    }

    /// `ItemPickedUp` (no poof).
    pub fn remove(&mut self, id: u32) {
        self.map.remove(&id);
    }

    /// Draws all entities + combat effects, tinted by the light at their
    /// cell like tiles and players.
    pub fn draw(&mut self, render_t: f64, now: f64, cam_top_left: Vec2, light: &LightEngine) {
        let (mouse_x, mouse_y) = mouse_position();
        let mut tooltip: Option<(String, f32, f32)> = None;
        for (&id, e) in self.map.iter_mut() {
            let pos = e.sample(render_t);
            e.last_pos = pos;
            let (w, h) = e.size();
            let px = pos.0 * TILE_SIZE - cam_top_left.x;
            let py = pos.1 * TILE_SIZE - cam_top_left.y;
            if px < -6.0 * TILE_SIZE
                || py < -6.0 * TILE_SIZE
                || px > screen_width() + 6.0 * TILE_SIZE
                || py > screen_height() + 6.0 * TILE_SIZE
            {
                continue;
            }
            let l = light.brightness_at(pos.0 + w / 2.0, pos.1 + h / 2.0);
            let vel = e.latest_vel();
            let flash = ((now - e.hurt_t) < FLASH_SECS) as u8 as f32;

            match &e.kind {
                Kind::Other(EntityKind::FallingSand) => {
                    draw_rectangle(
                        px,
                        py,
                        TILE_SIZE,
                        TILE_SIZE,
                        lit_color(tile_color(TileId::Sand), l),
                    );
                }
                Kind::Other(_) => {}
                Kind::Projectile(kind) => draw_projectile(*kind, px, py, w, h, vel, now, l),
                Kind::Enemy(kind) => {
                    draw_enemy(*kind, px, py, w, h, vel, now, l, flash, id);
                    // Health bar above damaged enemies, fading 3 s after the
                    // last hit.
                    let since = now - e.hurt_t;
                    if e.hp < e.max_hp && since < HEALTH_BAR_SECS + HEALTH_BAR_FADE {
                        let alpha = if since <= HEALTH_BAR_SECS {
                            1.0
                        } else {
                            1.0 - ((since - HEALTH_BAR_SECS) / HEALTH_BAR_FADE) as f32
                        };
                        draw_health_bar(
                            px + w * TILE_SIZE / 2.0,
                            py - 7.0,
                            e.hp as f32 / e.max_hp as f32,
                            alpha,
                        );
                    }
                }
                Kind::Item { item, count } => {
                    let phase = id as f32 * 0.7;
                    let cx = px + w / 2.0 * TILE_SIZE;
                    let cy = py
                        + h / 2.0 * TILE_SIZE
                        + (now as f32 * BOB_HZ * std::f32::consts::TAU + phase).sin() * BOB_PX;
                    draw_rectangle_ex(
                        cx,
                        cy,
                        ITEM_PX,
                        ITEM_PX,
                        DrawRectangleParams {
                            offset: vec2(0.5, 0.5),
                            rotation: now as f32 * SPIN_HZ * std::f32::consts::TAU + phase,
                            color: lit_color(item_color(*item), l),
                        },
                    );
                    if tooltip.is_none()
                        && (mouse_x - cx).abs() <= TILE_SIZE * 0.5
                        && (mouse_y - cy).abs() <= TILE_SIZE * 0.5
                    {
                        let name = item.data().name;
                        let text = if *count > 1 {
                            format!("{name} ({count})")
                        } else {
                            name.to_string()
                        };
                        tooltip = Some((text, cx, cy));
                    }
                }
            }
        }

        self.draw_poofs(now, cam_top_left);
        self.draw_floaties(now, cam_top_left);
        if let Some((text, cx, cy)) = tooltip {
            shadow_text(&text, cx + 10.0, cy - 10.0, 18.0, WHITE);
        }
    }

    fn draw_floaties(&mut self, now: f64, tl: Vec2) {
        self.floaties.retain(|f| now - f.t0 <= FLOATY_SECS);
        for f in &self.floaties {
            let age = ((now - f.t0) / FLOATY_SECS) as f32;
            let alpha = (1.0 - age).clamp(0.0, 1.0);
            let (size, color) = if f.crit {
                (30.0, Color::new(1.0, 0.85, 0.2, alpha)) // crits: larger, yellow
            } else {
                (21.0, Color::new(1.0, 0.45, 0.35, alpha))
            };
            let x = f.pos.0 * TILE_SIZE - tl.x;
            let y = f.pos.1 * TILE_SIZE - tl.y - age * FLOATY_RISE_PX;
            let dims = measure_text(&f.text, None, size as u16, 1.0);
            draw_text(
                &f.text,
                x - dims.width / 2.0 + 1.0,
                y + 1.0,
                size,
                Color::new(0.0, 0.0, 0.0, 0.6 * alpha),
            );
            draw_text(&f.text, x - dims.width / 2.0, y, size, color);
        }
    }

    fn draw_poofs(&mut self, now: f64, tl: Vec2) {
        self.poofs.retain(|p| now - p.t0 <= POOF_SECS);
        for p in &self.poofs {
            let age = ((now - p.t0) / POOF_SECS) as f32;
            let alpha = (1.0 - age).clamp(0.0, 1.0);
            for i in 0..POOF_PARTICLES {
                let h = hash2(p.seed, i);
                let angle = (h & 0xFF) as f32 / 255.0 * std::f32::consts::TAU;
                let speed = 18.0 + ((h >> 8) & 0x3F) as f32;
                let r = 2.0 + ((h >> 14) & 0x3) as f32;
                let d = age * speed;
                let x = p.pos.0 * TILE_SIZE - tl.x + angle.cos() * d;
                let y = p.pos.1 * TILE_SIZE - tl.y + angle.sin() * d + age * age * 24.0;
                draw_circle(
                    x,
                    y,
                    r * (1.0 - age * 0.5),
                    Color::new(p.color.r, p.color.g, p.color.b, alpha * 0.9),
                );
            }
        }
    }
}

impl Default for Entities {
    fn default() -> Self {
        Entities::new()
    }
}

fn hash2(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9E37_79B9) ^ b.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^ (h >> 16)
}

// ---- Enemy sprites -----------------------------------------------------------

const SICKLE_BODY: Color = Color::new(0.55, 0.25, 0.75, 1.0);

fn enemy_body_color(kind: EnemyKind) -> Color {
    match kind {
        EnemyKind::GreenSlime => Color::new(0.30, 0.85, 0.35, 0.78),
        EnemyKind::BlueSlime => Color::new(0.30, 0.50, 0.95, 0.78),
        EnemyKind::LavaSlime => Color::new(0.95, 0.45, 0.15, 0.85),
        EnemyKind::Zombie => Color::new(0.45, 0.62, 0.40, 1.0),
        EnemyKind::Skeleton => Color::new(0.88, 0.87, 0.80, 1.0),
        EnemyKind::DemonEye => Color::new(0.93, 0.90, 0.88, 1.0),
        EnemyKind::CaveBat => Color::new(0.42, 0.34, 0.45, 1.0),
        EnemyKind::AshDemon => Color::new(0.25, 0.18, 0.28, 1.0),
        EnemyKind::Watchling => Color::new(0.85, 0.75, 0.75, 1.0),
    }
}

/// Applies the white hit flash by lerping toward white.
fn flashed(c: Color, flash: f32, l: f32) -> Color {
    let c = lit_color(c, l.max(flash)); // a flash is visible even in the dark
    Color::new(
        c.r + (1.0 - c.r) * flash,
        c.g + (1.0 - c.g) * flash,
        c.b + (1.0 - c.b) * flash,
        c.a,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_enemy(
    kind: EnemyKind,
    px: f32,
    py: f32,
    w_tiles: f32,
    h_tiles: f32,
    vel: (f32, f32),
    now: f64,
    l: f32,
    flash: f32,
    id: u32,
) {
    let w = w_tiles * TILE_SIZE;
    let h = h_tiles * TILE_SIZE;
    let body = flashed(enemy_body_color(kind), flash, l);
    let dark = flashed(Color::new(0.08, 0.08, 0.10, 1.0), flash, l);
    // Face by horizontal velocity; default right when nearly still.
    let facing = if vel.0 < -0.05 { -1.0 } else { 1.0 };
    match kind {
        EnemyKind::GreenSlime | EnemyKind::BlueSlime | EnemyKind::LavaSlime => {
            // Squish on the ground (vel.y ≈ 0), stretch in flight.
            let stretch = (vel.1.abs() / 26.0).clamp(0.0, 0.45);
            let grounded = vel.1.abs() < 0.5;
            let (sx, sy) = if grounded {
                let wob = ((now * 6.0 + id as f64).sin() * 0.06) as f32;
                (1.12 + wob, 0.82 - wob)
            } else {
                (1.0 - stretch * 0.4, 1.0 + stretch)
            };
            let (bw, bh) = (w * sx, h * sy);
            let (cx, by) = (px + w / 2.0, py + h); // bottom-center anchored
            draw_rectangle(cx - bw / 2.0, by - bh, bw, bh * 0.72, body);
            draw_circle(cx, by - bh * 0.72, bw * 0.5 * 0.99, body);
            // Eyes look where it's going.
            let ex = cx + facing * bw * 0.16;
            draw_circle(ex - 3.0, by - bh * 0.62, 1.8, dark);
            draw_circle(ex + 3.0, by - bh * 0.62, 1.8, dark);
            if kind == EnemyKind::LavaSlime {
                // Embers.
                let g = ((now * 5.0).sin() * 0.5 + 0.5) as f32;
                draw_circle(
                    cx,
                    by - bh * 0.4,
                    2.0,
                    Color::new(1.0, 0.8, 0.3, 0.5 + 0.4 * g),
                );
            }
        }
        EnemyKind::Zombie | EnemyKind::Skeleton => {
            // Humanoid: head, torso, two shuffling legs.
            let leg_h = h * 0.27;
            let phase = ((now * 5.0 + id as f64) % 1.0) < 0.5;
            let walking = vel.0.abs() > 0.3;
            let (off_l, off_r) = if walking {
                if phase {
                    (-2.0, 2.0)
                } else {
                    (2.0, -2.0)
                }
            } else {
                (0.0, 0.0)
            };
            let pants = if kind == EnemyKind::Zombie {
                flashed(Color::new(0.25, 0.30, 0.24, 1.0), flash, l)
            } else {
                body
            };
            draw_rectangle(px + 2.0 + off_l, py + h - leg_h, 5.0, leg_h, pants);
            draw_rectangle(px + w - 7.0 + off_r, py + h - leg_h, 5.0, leg_h, pants);
            draw_rectangle(px + 1.5, py + h * 0.26, w - 3.0, h - leg_h - h * 0.26, body);
            draw_circle(px + w / 2.0, py + h * 0.15, w * 0.36, body);
            if kind == EnemyKind::Skeleton {
                // Eye sockets + rib hints.
                draw_circle(px + w / 2.0 - 2.5, py + h * 0.14, 1.6, dark);
                draw_circle(px + w / 2.0 + 2.5, py + h * 0.14, 1.6, dark);
                for i in 0..3 {
                    let y = py + h * 0.38 + i as f32 * 5.0;
                    draw_line(px + 4.0, y, px + w - 4.0, y, 1.0, dark);
                }
            } else {
                // Zombie: lurching arms + sunken eye.
                draw_circle(px + w / 2.0 + facing * 3.0, py + h * 0.14, 1.7, dark);
                let ay = py + h * 0.32 + if phase { 1.0 } else { -1.0 };
                let ax = if facing > 0.0 { px + w - 1.0 } else { px - 4.0 };
                draw_rectangle(ax, ay, 5.0, 3.5, body);
            }
        }
        EnemyKind::DemonEye | EnemyKind::Watchling => {
            // A flying eyeball: sclera, veins, iris + pupil toward velocity.
            let r = w / 2.0;
            let (cx, cy) = (px + r, py + h / 2.0);
            draw_circle(cx, cy, r, body);
            let dir = vec2(vel.0, vel.1).normalize_or_zero();
            let vein = flashed(Color::new(0.75, 0.20, 0.18, 1.0), flash, l);
            for i in 0..4 {
                let a = i as f32 * 1.5 + (id as f32 * 0.7);
                draw_line(
                    cx + a.cos() * r * 0.35,
                    cy + a.sin() * r * 0.35,
                    cx + a.cos() * r * 0.92,
                    cy + a.sin() * r * 0.92,
                    1.2,
                    vein,
                );
            }
            let iris = flashed(Color::new(0.55, 0.12, 0.12, 1.0), flash, l);
            draw_circle(cx + dir.x * r * 0.42, cy + dir.y * r * 0.42, r * 0.45, iris);
            draw_circle(cx + dir.x * r * 0.5, cy + dir.y * r * 0.5, r * 0.2, dark);
            // Trailing optic tendrils.
            draw_line(
                cx - dir.x * r,
                cy - dir.y * r,
                cx - dir.x * (r + 6.0),
                cy - dir.y * (r + 6.0) + 2.0,
                2.0,
                vein,
            );
        }
        EnemyKind::CaveBat => {
            let (cx, cy) = (px + w / 2.0, py + h / 2.0);
            let flap = ((now * 14.0 + id as f64).sin()) as f32;
            draw_circle(cx, cy, w * 0.32, body);
            // Wings: two triangles flapping.
            let wing = flashed(Color::new(0.30, 0.24, 0.34, 1.0), flash, l);
            let wy = cy - flap * 5.0;
            draw_triangle(
                vec2(cx - 2.0, cy),
                vec2(cx - w * 0.8, wy - 3.0),
                vec2(cx - w * 0.5, cy + 3.0),
                wing,
            );
            draw_triangle(
                vec2(cx + 2.0, cy),
                vec2(cx + w * 0.8, wy - 3.0),
                vec2(cx + w * 0.5, cy + 3.0),
                wing,
            );
            draw_circle(cx - 1.5, cy - 1.0, 1.0, flashed(RED, flash, l));
            draw_circle(cx + 1.5, cy - 1.0, 1.0, flashed(RED, flash, l));
        }
        EnemyKind::AshDemon => {
            let (cx, cy) = (px + w / 2.0, py + h / 2.0);
            let flap = ((now * 6.0 + id as f64).sin()) as f32;
            // Smoldering glow behind the silhouette.
            let g = 0.5 + 0.3 * ((now * 3.0).sin() as f32);
            draw_circle(cx, cy, w * 0.85, Color::new(0.9, 0.3, 0.1, 0.10 + 0.06 * g));
            // Wings.
            let wing = flashed(Color::new(0.16, 0.10, 0.18, 1.0), flash, l);
            let wy = cy - 6.0 - flap * 7.0;
            draw_triangle(
                vec2(cx - 3.0, cy - 4.0),
                vec2(cx - w * 1.1, wy),
                vec2(cx - w * 0.5, cy + 6.0),
                wing,
            );
            draw_triangle(
                vec2(cx + 3.0, cy - 4.0),
                vec2(cx + w * 1.1, wy),
                vec2(cx + w * 0.5, cy + 6.0),
                wing,
            );
            // Body + horned head + ember eyes.
            draw_rectangle(px + w * 0.22, py + h * 0.22, w * 0.56, h * 0.66, body);
            draw_circle(cx, py + h * 0.18, w * 0.28, body);
            let horn = dark;
            draw_triangle(
                vec2(cx - 5.0, py + h * 0.10),
                vec2(cx - 9.0, py - 3.0),
                vec2(cx - 2.0, py + h * 0.06),
                horn,
            );
            draw_triangle(
                vec2(cx + 5.0, py + h * 0.10),
                vec2(cx + 9.0, py - 3.0),
                vec2(cx + 2.0, py + h * 0.06),
                horn,
            );
            let eye = Color::new(1.0, 0.55, 0.15, 1.0);
            draw_circle(cx - 3.0, py + h * 0.16, 1.7, eye);
            draw_circle(cx + 3.0, py + h * 0.16, 1.7, eye);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_projectile(
    kind: EntityKind,
    px: f32,
    py: f32,
    w_tiles: f32,
    h_tiles: f32,
    vel: (f32, f32),
    now: f64,
    l: f32,
) {
    let (cx, cy) = (
        px + w_tiles * TILE_SIZE / 2.0,
        py + h_tiles * TILE_SIZE / 2.0,
    );
    match kind {
        EntityKind::VoidSickleProjectile => {
            // A spinning void crescent.
            let spin = (now * 4.0 * std::f64::consts::TAU) as f32;
            let r = 7.0;
            draw_circle(cx, cy, r + 2.0, Color::new(0.45, 0.15, 0.65, 0.25));
            draw_rectangle_ex(
                cx,
                cy,
                r * 2.2,
                3.5,
                DrawRectangleParams {
                    offset: vec2(0.5, 0.5),
                    rotation: spin,
                    color: lit_color(SICKLE_BODY, l.max(0.6)), // self-lit
                },
            );
            draw_rectangle_ex(
                cx,
                cy,
                r * 2.2,
                3.5,
                DrawRectangleParams {
                    offset: vec2(0.5, 0.5),
                    rotation: spin + std::f32::consts::FRAC_PI_2,
                    color: lit_color(Color::new(0.75, 0.45, 0.9, 1.0), l.max(0.6)),
                },
            );
        }
        _ => {
            // Arrow: a shaft rotated along the velocity, tipped.
            let angle = vel.1.atan2(vel.0);
            let flaming = kind == EntityKind::FlamingArrowProjectile;
            let shaft = if flaming {
                Color::new(0.85, 0.45, 0.2, 1.0)
            } else {
                Color::new(0.72, 0.56, 0.36, 1.0)
            };
            draw_rectangle_ex(
                cx,
                cy,
                14.0,
                2.5,
                DrawRectangleParams {
                    offset: vec2(0.5, 0.5),
                    rotation: angle,
                    color: lit_color(shaft, if flaming { l.max(0.7) } else { l }),
                },
            );
            let tip = vec2(angle.cos(), angle.sin()) * 7.0;
            draw_circle(
                cx + tip.x,
                cy + tip.y,
                2.2,
                lit_color(Color::new(0.8, 0.8, 0.85, 1.0), l),
            );
            if flaming {
                let g = ((now * 9.0).sin() * 0.5 + 0.5) as f32;
                draw_circle(cx, cy, 4.0 + g * 2.0, Color::new(1.0, 0.6, 0.2, 0.35));
            }
        }
    }
}

/// Small floating health bar (enemies; also reused over hurt remote
/// players): centered on `cx`, red→green fill by `frac`.
pub(crate) fn draw_health_bar(cx: f32, y: f32, frac: f32, alpha: f32) {
    let w = 30.0;
    let h = 4.0;
    let x = cx - w / 2.0;
    draw_rectangle(
        x - 1.0,
        y - 1.0,
        w + 2.0,
        h + 2.0,
        Color::new(0.0, 0.0, 0.0, 0.55 * alpha),
    );
    draw_rectangle(x, y, w, h, Color::new(0.35, 0.05, 0.05, 0.9 * alpha));
    let g = frac.clamp(0.0, 1.0);
    let color = Color::new(1.0 - g * 0.8, 0.15 + g * 0.65, 0.12, 0.95 * alpha);
    draw_rectangle(x, y, w * g, h, color);
}
