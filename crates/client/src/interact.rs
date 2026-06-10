//! Mouse-driven world interaction: tile aiming, hold-LMB mining at the held
//! tool's §4.1 cadence, RMB placing/doors/chests, and the mining-crack
//! overlay fed by `ServerMessage::BlockCrack`.
//!
//! Everything here is *intent only* — the server revalidates reach, swing
//! rate, and placement rules; this module just avoids sending obviously
//! invalid intents (out of reach, no target).

use std::collections::HashMap;

use macroquad::prelude::*;

use ferraria_shared::items::{InvSlot, ItemId, Placement, BARE_HAND_USE_SECS};
use ferraria_shared::protocol::ClientMessage;
use ferraria_shared::tiles::{TileId, ToolKind, WallId, TILE_DAMAGE_RESET_SECS};
use ferraria_shared::world::World;
use ferraria_shared::{tile_in_reach, TILE_SIZE};

use crate::net::WsClient;

/// Hold-RMB placement repeat. Client-side UX pacing only — the server
/// validates every placement independently.
const PLACE_REPEAT_SECS: f32 = 0.15;

/// Crack overlay drawing.
const CRACK_COLOR: Color = Color::new(0.05, 0.05, 0.05, 0.85);
/// Aim cursor colors: in reach vs out of reach.
const AIM_OK: Color = Color::new(1.0, 1.0, 0.85, 0.85);
const AIM_FAR: Color = Color::new(0.95, 0.25, 0.2, 0.85);

/// Mining/placing input state + the crack overlay mirror.
pub struct Interact {
    /// Per-cell crack: damage fraction (0–255) and arrival time (cracks
    /// expire after the §2 5 s damage decay, mirroring the server).
    cracks: HashMap<(u32, u32), (u8, f64)>,
    swing_cd: f32,
    place_cd: f32,
}

impl Interact {
    pub fn new() -> Interact {
        Interact {
            cracks: HashMap::new(),
            swing_cd: 0.0,
            place_cd: 0.0,
        }
    }

    pub fn on_block_crack(&mut self, x: u32, y: u32, damage_frac: u8, now: f64) {
        self.cracks.insert((x, y), (damage_frac, now));
    }

    /// Any authoritative change to a cell clears its crack overlay.
    pub fn on_tile_changed(&mut self, x: u32, y: u32) {
        self.cracks.remove(&(x, y));
    }

    /// The world tile under the mouse (`None` outside the world).
    pub fn aim(world: &World, cam_top_left: Vec2) -> Option<(u32, u32)> {
        let (mx, my) = mouse_position();
        let wx = (mx + cam_top_left.x) / TILE_SIZE;
        let wy = (my + cam_top_left.y) / TILE_SIZE;
        if wx < 0.0 || wy < 0.0 {
            return None;
        }
        let (x, y) = (wx as u32, wy as u32);
        world.in_bounds(x, y).then_some((x, y))
    }

    /// Handles this frame's mouse input: hold-LMB mining at the held tool's
    /// cadence, RMB door/chest interaction and hold-to-place.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        ws: &WsClient,
        world: &World,
        center: (f32, f32),
        slots: &[Option<InvSlot>],
        selected: u8,
        aim: Option<(u32, u32)>,
        dt: f32,
    ) {
        self.swing_cd = (self.swing_cd - dt).max(0.0);
        self.place_cd = (self.place_cd - dt).max(0.0);
        let Some((x, y)) = aim else {
            return;
        };
        if !tile_in_reach(center, x, y) {
            return; // red highlight already says why
        }
        let held = slots
            .get(selected as usize)
            .copied()
            .flatten()
            .map(|s| s.item);
        let t = world.tile(x, y);

        // LMB: swing at the aimed cell — the foreground tile, or the wall
        // behind it when holding a hammer.
        if is_mouse_button_down(MouseButton::Left) && self.swing_cd <= 0.0 {
            let hammer = held
                .and_then(|i| i.data().tool)
                .is_some_and(|t| t.kind == ToolKind::Hammer);
            let msg = if t.id != TileId::Air {
                Some(ClientMessage::HitTile { x, y })
            } else if hammer && t.wall != WallId::Air {
                Some(ClientMessage::HitWall { x, y })
            } else {
                None
            };
            if let Some(msg) = msg {
                ws.send(&msg);
                self.swing_cd = use_secs(held);
            }
        }

        // RMB press: doors toggle, chests open (the panel is the inventory
        // branch's job — we only send the intent).
        if is_mouse_button_pressed(MouseButton::Right) {
            match t.id {
                TileId::Door => {
                    ws.send(&ClientMessage::ToggleDoor { x, y });
                    return;
                }
                TileId::Chest => {
                    let (ox, oy) = world.multitile_origin(x, y);
                    ws.send(&ClientMessage::OpenChest { x: ox, y: oy });
                    return;
                }
                _ => {}
            }
        }

        // RMB hold: place the held placeable.
        if is_mouse_button_down(MouseButton::Right) && self.place_cd <= 0.0 {
            let msg = match held.and_then(|i| i.data().places) {
                Some(Placement::Tile(_)) if t.id == TileId::Air => Some(ClientMessage::PlaceTile {
                    x,
                    y,
                    hotbar_slot: selected,
                }),
                Some(Placement::Wall(_)) if t.wall == WallId::Air => {
                    Some(ClientMessage::PlaceWall {
                        x,
                        y,
                        hotbar_slot: selected,
                    })
                }
                _ => None,
            };
            if let Some(msg) = msg {
                ws.send(&msg);
                self.place_cd = PLACE_REPEAT_SECS;
            }
        }
    }

    /// Outlines the aimed tile: warm white in reach, red outside (§8 reach).
    pub fn draw_aim(&self, aim: Option<(u32, u32)>, center: (f32, f32), cam_top_left: Vec2) {
        let Some((x, y)) = aim else {
            return;
        };
        let color = if tile_in_reach(center, x, y) {
            AIM_OK
        } else {
            AIM_FAR
        };
        draw_rectangle_lines(
            x as f32 * TILE_SIZE - cam_top_left.x,
            y as f32 * TILE_SIZE - cam_top_left.y,
            TILE_SIZE,
            TILE_SIZE,
            2.0,
            color,
        );
    }

    /// Draws the 3-stage crack overlay and prunes expired entries.
    pub fn draw_cracks(&mut self, now: f64, cam_top_left: Vec2) {
        self.cracks
            .retain(|_, &mut (_, at)| now - at <= TILE_DAMAGE_RESET_SECS as f64);
        for (&(x, y), &(frac, _)) in &self.cracks {
            let px = x as f32 * TILE_SIZE - cam_top_left.x;
            let py = y as f32 * TILE_SIZE - cam_top_left.y;
            if px < -TILE_SIZE || py < -TILE_SIZE || px > screen_width() || py > screen_height() {
                continue;
            }
            let s = TILE_SIZE;
            let stage = 1 + (frac as u32 * 3 / 256).min(2); // 1..=3
            draw_line(
                px + s * 0.2,
                py + s * 0.3,
                px + s * 0.6,
                py + s * 0.75,
                1.5,
                CRACK_COLOR,
            );
            if stage >= 2 {
                draw_line(
                    px + s * 0.75,
                    py + s * 0.15,
                    px + s * 0.45,
                    py + s * 0.6,
                    1.5,
                    CRACK_COLOR,
                );
                draw_line(
                    px + s * 0.3,
                    py + s * 0.55,
                    px + s * 0.15,
                    py + s * 0.85,
                    1.5,
                    CRACK_COLOR,
                );
            }
            if stage >= 3 {
                draw_line(
                    px + s * 0.55,
                    py + s * 0.7,
                    px + s * 0.85,
                    py + s * 0.9,
                    1.5,
                    CRACK_COLOR,
                );
                draw_line(
                    px + s * 0.65,
                    py + s * 0.4,
                    px + s * 0.9,
                    py + s * 0.55,
                    1.5,
                    CRACK_COLOR,
                );
                draw_line(
                    px + s * 0.1,
                    py + s * 0.15,
                    px + s * 0.35,
                    py + s * 0.35,
                    1.5,
                    CRACK_COLOR,
                );
            }
        }
    }
}

impl Default for Interact {
    fn default() -> Self {
        Interact::new()
    }
}

/// Swing interval for the held item — mirrors the server's rate limiter
/// (tools and weapons use their §4.1 use time, bare hands the canonized
/// default), so legit clients never get swings rejected.
fn use_secs(held: Option<ItemId>) -> f32 {
    held.and_then(|i| {
        let d = i.data();
        d.tool.map(|t| t.use_secs).or(d.weapon.map(|w| w.use_secs))
    })
    .unwrap_or(BARE_HAND_USE_SECS)
    .max(1.0 / 60.0)
}
