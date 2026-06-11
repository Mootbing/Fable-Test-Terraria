//! Mouse-driven world interaction: tile aiming, hold-LMB mining (or weapon
//! swings/bow shots, §4.1) at the held item's use cadence, RMB
//! placing/doors/chests, and the mining-crack overlay fed by
//! `ServerMessage::BlockCrack`.
//!
//! Everything here is *intent only* — the server revalidates reach, swing
//! rate, and placement rules; this module just avoids sending obviously
//! invalid intents (out of reach, no target).

use std::collections::HashMap;

use macroquad::prelude::*;

use ferraria_shared::items::{
    inventory, InvSlot, ItemId, Placement, WeaponKind, BARE_HAND_USE_SECS,
};
use ferraria_shared::protocol::ClientMessage;
use ferraria_shared::tiles::{state, TileId, ToolKind, WallId, TILE_DAMAGE_RESET_SECS};
use ferraria_shared::world::World;
use ferraria_shared::{tile_in_reach, TILE_SIZE};

use crate::light::LightEngine;
use crate::net::WsClient;

/// Hold-RMB placement repeat. Client-side UX pacing only — the server
/// validates every placement independently.
const PLACE_REPEAT_SECS: f32 = 0.15;

/// Crack overlay drawing.
const CRACK_COLOR: Color = Color::new(0.05, 0.05, 0.05, 0.85);
/// Aim cursor colors: in reach vs out of reach.
const AIM_OK: Color = Color::new(1.0, 1.0, 0.85, 0.85);
const AIM_FAR: Color = Color::new(0.95, 0.25, 0.2, 0.85);

/// Mining/placing input state + the crack overlay mirror + the combat-use
/// state (swing/bow animation timing for the own player).
pub struct Interact {
    /// Per-cell crack: damage fraction (0–255) and arrival time (cracks
    /// expire after the §2 5 s damage decay, mirroring the server).
    cracks: HashMap<(u32, u32), (u8, f64)>,
    swing_cd: f32,
    place_cd: f32,
    /// Live use animation: (start time, duration, is_bow).
    swing_anim: Option<(f64, f64, bool)>,
}

impl Interact {
    pub fn new() -> Interact {
        Interact {
            cracks: HashMap::new(),
            swing_cd: 0.0,
            place_cd: 0.0,
            swing_anim: None,
        }
    }

    /// Progress (0..1) of the live swing/draw animation, and whether it's a
    /// bow. `None` once it finished.
    pub fn swing(&self, now: f64) -> Option<(f32, bool)> {
        let (start, dur, bow) = self.swing_anim?;
        let p = ((now - start) / dur) as f32;
        (p < 1.0).then_some((p.max(0.0), bow))
    }

    /// Starts the held item's shared §4.1 use cooldown (one limiter across
    /// mining and weapon swings, like the server's) and its animation.
    fn start_use(&mut self, held: Option<ItemId>, now: f64, is_bow: bool) {
        let secs = use_secs(held);
        self.swing_cd = secs;
        self.swing_anim = Some((now, secs as f64, is_bow));
    }

    pub fn on_block_crack(&mut self, x: u32, y: u32, damage_frac: u8, now: f64) {
        self.cracks.insert((x, y), (damage_frac, now));
    }

    /// Any authoritative change to a cell clears its crack overlay.
    pub fn on_tile_changed(&mut self, x: u32, y: u32) {
        self.cracks.remove(&(x, y));
    }

    /// Mouse position in world tile coordinates (unclamped — also the
    /// `UseItem` aim, which doesn't need to land on a world cell).
    pub fn mouse_world(cam_top_left: Vec2) -> (f32, f32) {
        let (mx, my) = mouse_position();
        (
            (mx + cam_top_left.x) / TILE_SIZE,
            (my + cam_top_left.y) / TILE_SIZE,
        )
    }

    /// The world tile under the mouse (`None` outside the world).
    pub fn aim(world: &World, cam_top_left: Vec2) -> Option<(u32, u32)> {
        let (wx, wy) = Interact::mouse_world(cam_top_left);
        if wx < 0.0 || wy < 0.0 {
            return None;
        }
        let (x, y) = (wx as u32, wy as u32);
        world.in_bounds(x, y).then_some((x, y))
    }

    /// Handles this frame's mouse input: hold-LMB mining or weapon use at
    /// the held item's §4.1 cadence, RMB door/chest interaction and
    /// hold-to-place. `aim_pos` is the raw mouse position in world tiles
    /// (the `UseItem` aim — valid even off any world cell).
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        ws: &WsClient,
        world: &World,
        center: (f32, f32),
        slots: &[Option<InvSlot>],
        selected: u8,
        aim: Option<(u32, u32)>,
        aim_pos: (f32, f32),
        now: f64,
        dt: f32,
    ) {
        self.swing_cd = (self.swing_cd - dt).max(0.0);
        self.place_cd = (self.place_cd - dt).max(0.0);
        let held = slots
            .get(selected as usize)
            .copied()
            .flatten()
            .map(|s| s.item);
        let data = held.map(|i| i.data());
        let tool = data.and_then(|d| d.tool);
        // Swingable weapon rows only — `Arrow` rows are ammo (§4.1).
        let weapon = data
            .and_then(|d| d.weapon)
            .filter(|w| matches!(w.kind, WeaponKind::Melee | WeaponKind::Bow));

        // LMB resolution mirrors the server, where `HitTile`/`HitWall` mine
        // and `UseItem` runs the weapon row, all behind one §4.1 rate
        // limit: items with both rows (pickaxes also deal melee damage)
        // prefer the tool when aiming at a minable cell in reach;
        // weapon-only items never send `HitTile`. The weapon path has no
        // aim/reach gate — `use_item` enforces none (the melee arc anchors
        // on the player, bows fire toward any aim), so air and far enemies
        // are fine. Consumables (§8 healing potions, Life Crystals, boss
        // summons) have neither row and fall through to a bare `UseItem`.
        if is_mouse_button_down(MouseButton::Left) && self.swing_cd <= 0.0 {
            let weapon_only = weapon.is_some() && tool.is_none();
            let tool_msg = aim
                .filter(|&(x, y)| !weapon_only && tile_in_reach(center, x, y))
                .and_then(|(x, y)| {
                    let t = world.tile(x, y);
                    let hammer = tool.is_some_and(|t| t.kind == ToolKind::Hammer);
                    if t.id != TileId::Air {
                        Some(ClientMessage::HitTile { x, y })
                    } else if hammer && t.wall != WallId::Air {
                        Some(ClientMessage::HitWall { x, y })
                    } else {
                        None
                    }
                });
            if let Some(msg) = tool_msg {
                ws.send(&msg);
                self.start_use(held, now, false);
            } else if let Some(w) = weapon {
                // Mirror the server's ammo lookup: a bow with no arrows in
                // the carry slots fires nothing — don't animate a phantom.
                let is_bow = w.kind == WeaponKind::Bow;
                if !is_bow || has_arrows(slots) {
                    ws.send(&ClientMessage::UseItem {
                        slot: selected,
                        aim: aim_pos,
                    });
                    self.start_use(held, now, is_bow);
                }
            } else if data.is_some_and(|d| d.consumable.is_some()) {
                // The server validates the actual effect (Potion Sickness,
                // already-full HP, the §8 Life Crystal cap) — we just send
                // the intent and pace re-sends at the §4.1 use cadence.
                ws.send(&ClientMessage::UseItem {
                    slot: selected,
                    aim: aim_pos,
                });
                self.start_use(held, now, false);
            }
        }

        // Everything below needs an in-reach world cell under the mouse.
        let Some((x, y)) = aim else {
            return;
        };
        if !tile_in_reach(center, x, y) {
            return; // red highlight already says why
        }
        let t = world.tile(x, y);

        // RMB press: doors toggle, chests open (the chest panel itself is
        // `ui::inventory`'s job — we only send the intent), and a held
        // Bottle goes onto a Table/Workbench cell (the §4.4 Bottle station;
        // wire-wise a normal `PlaceTile`).
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
                TileId::Table | TileId::Workbench
                    if held == Some(ItemId::Bottle) && t.state & state::BOTTLE_ON_TOP == 0 =>
                {
                    ws.send(&ClientMessage::PlaceTile {
                        x,
                        y,
                        hotbar_slot: selected,
                    });
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

    /// Draws the 3-stage crack overlay and prunes expired entries. Crack
    /// alpha scales with the tile's light so cracks fade into darkness with
    /// the tile they're on instead of floating over black cells.
    pub fn draw_cracks(&mut self, now: f64, cam_top_left: Vec2, light: &LightEngine) {
        self.cracks
            .retain(|_, &mut (_, at)| now - at <= TILE_DAMAGE_RESET_SECS as f64);
        for (&(x, y), &(frac, _)) in &self.cracks {
            let px = x as f32 * TILE_SIZE - cam_top_left.x;
            let py = y as f32 * TILE_SIZE - cam_top_left.y;
            if px < -TILE_SIZE || py < -TILE_SIZE || px > screen_width() || py > screen_height() {
                continue;
            }
            let l = light.brightness_at(x as f32 + 0.5, y as f32 + 0.5);
            if l <= 0.0 {
                continue; // pitch black: the tile itself isn't visible
            }
            let crack = Color::new(
                CRACK_COLOR.r,
                CRACK_COLOR.g,
                CRACK_COLOR.b,
                CRACK_COLOR.a * l,
            );
            let s = TILE_SIZE;
            let stage = 1 + (frac as u32 * 3 / 256).min(2); // 1..=3
            draw_line(
                px + s * 0.2,
                py + s * 0.3,
                px + s * 0.6,
                py + s * 0.75,
                1.5,
                crack,
            );
            if stage >= 2 {
                draw_line(
                    px + s * 0.75,
                    py + s * 0.15,
                    px + s * 0.45,
                    py + s * 0.6,
                    1.5,
                    crack,
                );
                draw_line(
                    px + s * 0.3,
                    py + s * 0.55,
                    px + s * 0.15,
                    py + s * 0.85,
                    1.5,
                    crack,
                );
            }
            if stage >= 3 {
                draw_line(
                    px + s * 0.55,
                    py + s * 0.7,
                    px + s * 0.85,
                    py + s * 0.9,
                    1.5,
                    crack,
                );
                draw_line(
                    px + s * 0.65,
                    py + s * 0.4,
                    px + s * 0.9,
                    py + s * 0.55,
                    1.5,
                    crack,
                );
                draw_line(
                    px + s * 0.1,
                    py + s * 0.15,
                    px + s * 0.35,
                    py + s * 0.35,
                    1.5,
                    crack,
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

/// Any arrow stack in the carry slots (hotbar + backpack) — the same
/// lookup the server's `fire_bow` makes before consuming ammo.
fn has_arrows(slots: &[Option<InvSlot>]) -> bool {
    slots
        .iter()
        .take(inventory::ARMOR_START)
        .flatten()
        .any(|s| {
            s.item
                .data()
                .weapon
                .is_some_and(|w| w.kind == WeaponKind::Arrow)
        })
}
