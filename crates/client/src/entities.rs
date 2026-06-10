//! Client mirror of server entities, keyed by id from
//! `ItemDropSpawn`/`EntitySpawn` and advanced by `EntityUpdate` snapshot
//! batches (interpolated ~100 ms in the past, like remote players).
//!
//! Item drops render as a bobbing, spinning 12 px swatch in the item's
//! color with a name tooltip on hover. Other entity kinds (enemies,
//! projectiles — later branches) are mirrored generically and simply not
//! drawn yet.

use std::collections::{HashMap, VecDeque};

use macroquad::prelude::*;

use ferraria_shared::items::ItemId;
use ferraria_shared::physics::hitbox;
use ferraria_shared::protocol::{EntityKind, EntityState};
use ferraria_shared::tiles::TileId;
use ferraria_shared::TILE_SIZE;

use crate::render::item_color;
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

/// What the client knows about a mirrored entity.
pub enum Kind {
    Item {
        item: ItemId,
        count: u16,
    },
    /// Falling sand draws as its tile; enemies/projectiles render in later
    /// branches.
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

    fn push(&mut self, snap: Snap) {
        if self.snaps.len() >= SNAPSHOT_BUFFER {
            self.snaps.pop_front();
        }
        self.snaps.push_back(snap);
    }
}

/// All mirrored entities.
pub struct Entities {
    map: HashMap<u32, Entity>,
}

impl Entities {
    pub fn new() -> Entities {
        Entities {
            map: HashMap::new(),
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

    /// Generic `EntitySpawn` (enemy/projectile kinds from later branches).
    pub fn spawn_other(
        &mut self,
        id: u32,
        kind: EntityKind,
        pos: (f32, f32),
        vel: (f32, f32),
        now: f64,
    ) {
        self.spawn(id, Kind::Other(kind), pos, vel, now);
    }

    fn spawn(&mut self, id: u32, kind: Kind, pos: (f32, f32), vel: (f32, f32), now: f64) {
        let mut snaps = VecDeque::new();
        snaps.push_back(Snap { t: now, pos, vel });
        self.map.insert(id, Entity { kind, snaps });
    }

    /// One `EntityUpdate` batch (ids we never saw spawn are skipped — their
    /// chunk isn't subscribed yet).
    pub fn update(&mut self, batch: &[EntityState], now: f64) {
        for s in batch {
            if let Some(e) = self.map.get_mut(&s.id) {
                e.push(Snap {
                    t: now,
                    pos: s.pos,
                    vel: s.vel,
                });
            }
        }
    }

    /// `EntityDespawn` / `ItemPickedUp`.
    pub fn remove(&mut self, id: u32) {
        self.map.remove(&id);
    }

    /// Draws all item drops (bob + spin), falling-sand tiles, and the name
    /// tooltip of the drop under the mouse, if any.
    pub fn draw(&mut self, render_t: f64, now: f64, cam_top_left: Vec2) {
        let (mouse_x, mouse_y) = mouse_position();
        let mut tooltip: Option<(String, f32, f32)> = None;
        let (w, h) = hitbox::ITEM_DROP;
        for (&id, e) in self.map.iter_mut() {
            // Falling sand (§2 tile 4) renders as the tile it is.
            if matches!(e.kind, Kind::Other(EntityKind::FallingSand)) {
                let pos = e.sample(render_t);
                draw_rectangle(
                    pos.0 * TILE_SIZE - cam_top_left.x,
                    pos.1 * TILE_SIZE - cam_top_left.y,
                    TILE_SIZE,
                    TILE_SIZE,
                    crate::render::tile_color(TileId::Sand),
                );
                continue;
            }
            let Kind::Item { item, count } = e.kind else {
                continue; // enemies/projectiles render in later branches
            };
            let pos = e.sample(render_t);
            let phase = id as f32 * 0.7;
            let cx = (pos.0 + w / 2.0) * TILE_SIZE - cam_top_left.x;
            let cy = (pos.1 + h / 2.0) * TILE_SIZE - cam_top_left.y
                + (now as f32 * BOB_HZ * std::f32::consts::TAU + phase).sin() * BOB_PX;
            if cx < -TILE_SIZE
                || cy < -TILE_SIZE
                || cx > screen_width() + TILE_SIZE
                || cy > screen_height() + TILE_SIZE
            {
                continue;
            }
            draw_rectangle_ex(
                cx,
                cy,
                ITEM_PX,
                ITEM_PX,
                DrawRectangleParams {
                    offset: vec2(0.5, 0.5),
                    rotation: now as f32 * SPIN_HZ * std::f32::consts::TAU + phase,
                    color: item_color(item),
                },
            );
            // Hover tooltip: generous half-tile radius around the sprite.
            if tooltip.is_none()
                && (mouse_x - cx).abs() <= TILE_SIZE * 0.5
                && (mouse_y - cy).abs() <= TILE_SIZE * 0.5
            {
                let name = item.data().name;
                let text = if count > 1 {
                    format!("{name} ({count})")
                } else {
                    name.to_string()
                };
                tooltip = Some((text, cx, cy));
            }
        }
        if let Some((text, cx, cy)) = tooltip {
            shadow_text(&text, cx + 10.0, cy - 10.0, 18.0, WHITE);
        }
    }
}

impl Default for Entities {
    fn default() -> Self {
        Entities::new()
    }
}
