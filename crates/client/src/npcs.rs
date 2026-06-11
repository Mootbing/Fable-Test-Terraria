//! Client mirror of the town NPCs (DESIGN §7): roster from
//! `ServerMessage::NpcList`, movement from the shared `EntityUpdate`
//! snapshot batches (same ids as the server entity store), and rendering —
//! three distinct primitive-built characters with idle/walk animation,
//! nameplates, and a right-click talk prompt when the player is in range.

use std::collections::{HashMap, VecDeque};

use macroquad::prelude::*;

use ferraria_shared::npc::{anim as npc_anim, NPC_TALK_RANGE};
use ferraria_shared::physics::{PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::{EntityState, NpcInfo, NpcKind};
use ferraria_shared::TILE_SIZE;

use crate::light::LightEngine;
use crate::render::lit_color;
use crate::ui::shadow_text;

/// On snapshot gaps, extrapolate along the last velocity at most this long
/// (matches remote players / entities).
const MAX_EXTRAPOLATION: f64 = 0.10;
const SNAPSHOT_BUFFER: usize = 64;
/// `NpcList` position vs mirrored history divergence beyond which the
/// history is reseeded (the `CORRECTION_SNAP_TILES` idiom; generous enough
/// that normal wander interpolation never trips it).
const NPC_SNAP_TILES: f32 = 2.0;

const NAME_SIZE: f32 = 18.0;

struct Snap {
    t: f64,
    pos: (f32, f32),
    vel: (f32, f32),
    state: u8,
}

/// One mirrored town NPC.
pub struct Npc {
    pub kind: NpcKind,
    pub name: String,
    #[allow(dead_code)] // housing indicator UI may use it later
    pub housed: bool,
    snaps: VecDeque<Snap>,
}

impl Npc {
    /// Interpolated AABB top-left + anim state at `render_t`.
    fn sample(&mut self, render_t: f64) -> ((f32, f32), u8) {
        while self.snaps.len() >= 2 && self.snaps[1].t <= render_t {
            self.snaps.pop_front();
        }
        let Some(a) = self.snaps.front() else {
            return ((0.0, 0.0), 0);
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
                    (
                        a.pos.0 + (b.pos.0 - a.pos.0) * f,
                        a.pos.1 + (b.pos.1 - a.pos.1) * f,
                    ),
                    b.state,
                )
            }
            None => {
                let dt = (render_t - a.t).clamp(0.0, MAX_EXTRAPOLATION) as f32;
                ((a.pos.0 + a.vel.0 * dt, a.pos.1 + a.vel.1 * dt), a.state)
            }
        }
    }

    fn push(&mut self, snap: Snap) {
        if self.snaps.len() >= SNAPSHOT_BUFFER {
            self.snaps.pop_front();
        }
        self.snaps.push_back(snap);
    }
}

/// All mirrored town NPCs, keyed by entity id.
pub struct Npcs {
    map: HashMap<u32, Npc>,
}

impl Npcs {
    pub fn new() -> Npcs {
        Npcs {
            map: HashMap::new(),
        }
    }

    /// `NpcList`: full roster replacement. Snapshot histories survive for
    /// ids that persist — unless the roster position disagrees with the
    /// mirror by more than [`NPC_SNAP_TILES`], which means the history is
    /// stale (e.g. the town chunk left the window while the NPC went home
    /// for the night) and interpolating from it would render a ghost; then
    /// the history is reseeded at the authoritative position.
    pub fn apply_list(&mut self, list: Vec<NpcInfo>, now: f64) {
        let mut next: HashMap<u32, Npc> = HashMap::new();
        for info in list {
            let mut npc = match self.map.remove(&info.id) {
                Some(existing) => existing,
                None => {
                    let mut snaps = VecDeque::new();
                    snaps.push_back(Snap {
                        t: now,
                        pos: info.pos,
                        vel: (0.0, 0.0),
                        state: npc_anim::FACING_RIGHT,
                    });
                    Npc {
                        kind: info.kind,
                        name: info.name.clone(),
                        housed: info.housed,
                        snaps,
                    }
                }
            };
            if let Some(last) = npc.snaps.back() {
                let (dx, dy) = (info.pos.0 - last.pos.0, info.pos.1 - last.pos.1);
                if dx * dx + dy * dy > NPC_SNAP_TILES * NPC_SNAP_TILES {
                    let state = last.state;
                    npc.snaps.clear();
                    npc.snaps.push_back(Snap {
                        t: now,
                        pos: info.pos,
                        vel: (0.0, 0.0),
                        state,
                    });
                }
            }
            npc.kind = info.kind;
            npc.name = info.name;
            npc.housed = info.housed;
            next.insert(info.id, npc);
        }
        self.map = next;
    }

    /// Shared `EntityUpdate` batches: ids we don't track are someone else's
    /// (item drops, enemies).
    pub fn update(&mut self, batch: &[EntityState], now: f64) {
        for s in batch {
            if let Some(npc) = self.map.get_mut(&s.id) {
                npc.push(Snap {
                    t: now,
                    pos: s.pos,
                    vel: s.vel,
                    state: s.state,
                });
            }
        }
    }

    pub fn remove(&mut self, id: u32) {
        self.map.remove(&id);
    }

    pub fn get(&self, id: u32) -> Option<&Npc> {
        self.map.get(&id)
    }

    /// The closest NPC within talk range of the player, if any.
    pub fn nearest_in_reach(&mut self, player_center: (f32, f32), render_t: f64) -> Option<u32> {
        let mut best: Option<(f32, u32)> = None;
        for (&id, npc) in self.map.iter_mut() {
            let (pos, _) = npc.sample(render_t);
            let c = (pos.0 + PLAYER_WIDTH / 2.0, pos.1 + PLAYER_HEIGHT / 2.0);
            let d2 = (c.0 - player_center.0).powi(2) + (c.1 - player_center.1).powi(2);
            if d2 <= NPC_TALK_RANGE * NPC_TALK_RANGE && best.is_none_or(|(bd, _)| d2 < bd) {
                best = Some((d2, id));
            }
        }
        best.map(|(_, id)| id)
    }

    /// The NPC whose AABB is under the world-space cursor (tile units) and
    /// who is within talk range of the player — the right-click talk target
    /// (§7.4). Closest to the player wins should sprites overlap.
    pub fn hovered_in_reach(
        &mut self,
        cursor: (f32, f32),
        player_center: (f32, f32),
        render_t: f64,
    ) -> Option<u32> {
        let mut best: Option<(f32, u32)> = None;
        for (&id, npc) in self.map.iter_mut() {
            let (pos, _) = npc.sample(render_t);
            let hovered = (pos.0..pos.0 + PLAYER_WIDTH).contains(&cursor.0)
                && (pos.1..pos.1 + PLAYER_HEIGHT).contains(&cursor.1);
            if !hovered {
                continue;
            }
            let c = (pos.0 + PLAYER_WIDTH / 2.0, pos.1 + PLAYER_HEIGHT / 2.0);
            let d2 = (c.0 - player_center.0).powi(2) + (c.1 - player_center.1).powi(2);
            if d2 <= NPC_TALK_RANGE * NPC_TALK_RANGE && best.is_none_or(|(bd, _)| d2 < bd) {
                best = Some((d2, id));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Whether `id` is still within talk range (panel auto-close).
    pub fn in_reach(&mut self, id: u32, player_center: (f32, f32), render_t: f64) -> bool {
        let Some(npc) = self.map.get_mut(&id) else {
            return false;
        };
        let (pos, _) = npc.sample(render_t);
        let c = (pos.0 + PLAYER_WIDTH / 2.0, pos.1 + PLAYER_HEIGHT / 2.0);
        let d2 = (c.0 - player_center.0).powi(2) + (c.1 - player_center.1).powi(2);
        d2 <= NPC_TALK_RANGE * NPC_TALK_RANGE
    }

    /// Draws every NPC: character sprite, nameplate, and the right-click
    /// talk prompt over the nearest in-range one.
    pub fn draw(
        &mut self,
        render_t: f64,
        now: f64,
        cam_top_left: Vec2,
        light: &LightEngine,
        player_center: (f32, f32),
    ) {
        let prompt = self.nearest_in_reach(player_center, render_t);
        for (&id, npc) in self.map.iter_mut() {
            let (pos, state) = npc.sample(render_t);
            let px = pos.0 * TILE_SIZE - cam_top_left.x;
            let py = pos.1 * TILE_SIZE - cam_top_left.y;
            if px < -3.0 * TILE_SIZE
                || py < -4.0 * TILE_SIZE
                || px > screen_width() + 3.0 * TILE_SIZE
                || py > screen_height() + 4.0 * TILE_SIZE
            {
                continue;
            }
            let l = light
                .brightness_at(pos.0 + PLAYER_WIDTH / 2.0, pos.1 + PLAYER_HEIGHT / 2.0)
                .clamp(0.0, 1.0);
            draw_npc(npc.kind, vec2(px, py), pos.0, state, now, l);

            // Nameplate.
            let w = PLAYER_WIDTH * TILE_SIZE;
            let dims = measure_text(&npc.name, None, NAME_SIZE as u16, 1.0);
            let nx = px + w * 0.5 - dims.width * 0.5;
            let ny = py - 8.0;
            draw_text(
                &npc.name,
                nx + 1.0,
                ny + 1.0,
                NAME_SIZE,
                Color::new(0.0, 0.0, 0.0, 0.6),
            );
            draw_text(
                &npc.name,
                nx,
                ny,
                NAME_SIZE,
                Color::new(0.85, 0.95, 0.6, 1.0),
            );

            // Speech indicator: a right-click hint bubble over the nearest
            // in-range NPC (talking is RMB on the NPC, §7.4).
            if prompt == Some(id) {
                let bob = (now as f32 * 2.0).sin() * 2.0;
                let bx = px + w * 0.5;
                let by = ny - 16.0 + bob;
                let label = "R-Click";
                let dims = measure_text(label, None, 16, 1.0);
                let bw = dims.width + 12.0;
                draw_rectangle(
                    bx - bw * 0.5,
                    by - 12.0,
                    bw,
                    16.0,
                    Color::new(0.0, 0.0, 0.0, 0.6),
                );
                draw_rectangle_lines(
                    bx - bw * 0.5,
                    by - 12.0,
                    bw,
                    16.0,
                    1.5,
                    Color::new(1.0, 1.0, 1.0, 0.5),
                );
                shadow_text(label, bx - dims.width * 0.5, by + 0.5, 16.0, WHITE);
            }
        }
    }
}

impl Default for Npcs {
    fn default() -> Self {
        Npcs::new()
    }
}

// ---- Character sprites (primitives; texture atlas comes later) ------------------

const SKIN: Color = Color::new(0.94, 0.78, 0.62, 1.0);
const EYE: Color = Color::new(0.08, 0.08, 0.10, 1.0);

const SAGE_ROBE: Color = Color::new(0.45, 0.32, 0.68, 1.0);
const SAGE_TRIM: Color = Color::new(0.72, 0.62, 0.30, 1.0);
const BOOK: Color = Color::new(0.55, 0.30, 0.20, 1.0);

const MERCHANT_COAT: Color = Color::new(0.55, 0.38, 0.22, 1.0);
const MERCHANT_SHIRT: Color = Color::new(0.85, 0.80, 0.65, 1.0);
const MERCHANT_HAT: Color = Color::new(0.30, 0.22, 0.14, 1.0);
const POUCH: Color = Color::new(0.90, 0.75, 0.25, 1.0);

const NURSE_DRESS: Color = Color::new(0.94, 0.94, 0.96, 1.0);
const NURSE_CROSS: Color = Color::new(0.85, 0.20, 0.22, 1.0);
const PANTS: Color = Color::new(0.24, 0.24, 0.36, 1.0);

/// Draws one NPC character at screen px `p` (AABB top-left). `world_x`
/// drives the 2-frame walk cycle; `state` carries the facing/walking bits.
fn draw_npc(kind: NpcKind, p: Vec2, world_x: f32, state: u8, now: f64, l: f32) {
    let w = PLAYER_WIDTH * TILE_SIZE; // 20 px
    let h = PLAYER_HEIGHT * TILE_SIZE; // 44 px
    let (x, y) = (p.x, p.y);
    let facing: f32 = if state & npc_anim::FACING_RIGHT != 0 {
        1.0
    } else {
        -1.0
    };
    let walking = state & npc_anim::WALKING != 0;
    // Walk cycle alternates every half tile of travel; idle gets a slow bob.
    let phase = ((world_x * 2.0).floor() as i64) & 1 == 0;
    let idle_bob = if walking {
        0.0
    } else {
        ((now * 1.5).sin() * 1.0) as f32
    };
    let skin = lit_color(SKIN, l);

    match kind {
        NpcKind::Sage => {
            // Full-length robe (no legs), swaying hem while walking.
            let robe = lit_color(SAGE_ROBE, l);
            let hem = if walking && phase { 2.0 } else { 0.0 };
            draw_rectangle(x + 2.0, y + 12.0 + idle_bob, w - 4.0, h - 12.0, robe);
            draw_rectangle(x + 1.0 - hem, y + h - 6.0, w - 2.0 + hem * 2.0, 6.0, robe);
            // Gold trim.
            draw_rectangle(
                x + w * 0.5 - 1.5,
                y + 12.0 + idle_bob,
                3.0,
                h - 12.0,
                lit_color(SAGE_TRIM, l),
            );
            // Hooded head.
            draw_circle(x + w * 0.5, y + 7.0 + idle_bob, 8.0, robe);
            draw_circle(x + w * 0.5 + facing, y + 8.0 + idle_bob, 5.5, skin);
            draw_circle(x + w * 0.5 + facing * 3.0, y + 7.0 + idle_bob, 1.5, EYE);
            // The book, held out in front.
            let bx = x + w * 0.5 + facing * 9.0;
            draw_rectangle(bx - 3.5, y + 20.0 + idle_bob, 7.0, 9.0, lit_color(BOOK, l));
            draw_rectangle(
                bx - 3.5,
                y + 24.0 + idle_bob,
                7.0,
                1.0,
                Color::new(0.95, 0.92, 0.85, l),
            );
        }
        NpcKind::Merchant => {
            draw_legs(x, y, w, h, walking, phase, lit_color(PANTS, l));
            // Coat over shirt.
            draw_rectangle(
                x + 2.0,
                y + 12.0 + idle_bob,
                w - 4.0,
                h - 24.0,
                lit_color(MERCHANT_COAT, l),
            );
            draw_rectangle(
                x + 6.0,
                y + 13.0 + idle_bob,
                w - 12.0,
                10.0,
                lit_color(MERCHANT_SHIRT, l),
            );
            // Coin pouch at the hip.
            draw_circle(
                x + w * 0.5 - facing * 7.0,
                y + h - 16.0,
                3.5,
                lit_color(POUCH, l),
            );
            // Head + wide-brim hat.
            draw_circle(x + w * 0.5, y + 7.0 + idle_bob, 6.5, skin);
            draw_circle(x + w * 0.5 + facing * 3.0, y + 6.0 + idle_bob, 1.5, EYE);
            let hat = lit_color(MERCHANT_HAT, l);
            draw_rectangle(x - 1.0, y + 1.0 + idle_bob, w + 2.0, 3.0, hat);
            draw_rectangle(x + 4.0, y - 5.0 + idle_bob, w - 8.0, 7.0, hat);
        }
        NpcKind::Nurse => {
            draw_legs(x, y, w, h, walking, phase, lit_color(NURSE_DRESS, l));
            // White dress with the red cross emblem.
            let dress = lit_color(NURSE_DRESS, l);
            draw_rectangle(x + 2.0, y + 12.0 + idle_bob, w - 4.0, h - 24.0, dress);
            let cross = lit_color(NURSE_CROSS, l);
            let (cx, cy) = (x + w * 0.5, y + 19.0 + idle_bob);
            draw_rectangle(cx - 1.5, cy - 4.5, 3.0, 9.0, cross);
            draw_rectangle(cx - 4.5, cy - 1.5, 9.0, 3.0, cross);
            // Head + nurse cap.
            draw_circle(x + w * 0.5, y + 7.0 + idle_bob, 6.5, skin);
            draw_circle(x + w * 0.5 + facing * 3.0, y + 6.0 + idle_bob, 1.5, EYE);
            draw_rectangle(x + 4.0, y - 1.0 + idle_bob, w - 8.0, 4.0, dress);
            draw_rectangle(cx - 1.0, y - 0.5 + idle_bob, 2.0, 3.0, cross);
        }
    }
}

/// Two-legged lower body with the standard walk wobble.
fn draw_legs(x: f32, y: f32, w: f32, h: f32, walking: bool, phase: bool, color: Color) {
    let leg_h = 12.0;
    let leg_y = y + h - leg_h;
    let (off_l, off_r) = if walking {
        if phase {
            (-2.0, 2.0)
        } else {
            (2.0, -2.0)
        }
    } else {
        (0.0, 0.0)
    };
    draw_rectangle(x + 3.0 + off_l, leg_y, 5.5, leg_h, color);
    draw_rectangle(x + w - 8.5 + off_r, leg_y, 5.5, leg_h, color);
}
