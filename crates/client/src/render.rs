//! All drawing: camera, sky, the tile world, and player sprites.
//!
//! Primitive shapes only (rects/circles/lines) — a texture atlas comes in a
//! later PR. Only camera-visible tiles are drawn, and nothing here allocates
//! per frame. Tile colors live in one table indexed by `TileId`; a cheap
//! per-position hash varies brightness so terrain isn't flat.

use macroquad::prelude::*;

use ferraria_shared::items::ItemId;
use ferraria_shared::physics::{PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::anim;
use ferraria_shared::tiles::{state, LiquidKind, Tile, TileId, WallId, LIQUID_MAX_LEVEL};
use ferraria_shared::world::{World, DAWN_TICK, DUSK_TICK};
use ferraria_shared::TILE_SIZE;

use crate::world_view::WorldView;

// ---- Camera -------------------------------------------------------------------

/// Exponential smoothing rate for the camera follow (per second).
const CAMERA_STIFFNESS: f32 = 8.0;

/// A pixel-space camera centered on a world point.
pub struct Camera {
    /// Center of the view, world pixels.
    pub center: Vec2,
}

impl Camera {
    pub fn new(center: Vec2) -> Camera {
        Camera { center }
    }

    /// Smoothly tracks `target` (world px), clamped to the world bounds.
    pub fn follow(&mut self, target: Vec2, world_px: Vec2, dt: f32) {
        let blend = 1.0 - (-CAMERA_STIFFNESS * dt).exp();
        self.center += (target - self.center) * blend;
        self.center = clamp_center(self.center, world_px);
    }

    /// Jump straight to `target` (initial placement).
    pub fn snap(&mut self, target: Vec2, world_px: Vec2) {
        self.center = clamp_center(target, world_px);
    }

    /// World-pixel coordinate of the top-left screen corner.
    pub fn top_left(&self) -> Vec2 {
        self.center - vec2(screen_width(), screen_height()) * 0.5
    }
}

fn clamp_center(center: Vec2, world_px: Vec2) -> Vec2 {
    let half = vec2(screen_width(), screen_height()) * 0.5;
    let clamp_axis = |c: f32, half: f32, world: f32| {
        if world <= half * 2.0 {
            world * 0.5 // world smaller than the screen: center it
        } else {
            c.clamp(half, world - half)
        }
    };
    vec2(
        clamp_axis(center.x, half.x, world_px.x),
        clamp_axis(center.y, half.y, world_px.y),
    )
}

// ---- Sky ------------------------------------------------------------------------

/// Sky light ramps over the 30 in-game minutes (1800 ticks) centered on
/// dawn/dusk (DESIGN §9/§10).
const SKY_RAMP_TICKS: f32 = 1800.0;

const SKY_DAY: (f32, f32, f32) = (0.45, 0.70, 0.92);
const SKY_NIGHT: (f32, f32, f32) = (0.03, 0.04, 0.11);
/// Warm dawn/dusk tint, blended in around the transitions.
const SKY_WARM: (f32, f32, f32) = (0.95, 0.55, 0.30);
const SKY_WARM_MAX: f32 = 0.40;

/// 0 at full night, 1 at full day, ramping across dawn/dusk.
fn daylight(time: u32) -> f32 {
    let t = time as f32;
    let half = SKY_RAMP_TICKS / 2.0;
    let rise = ((t - (DAWN_TICK as f32 - half)) / SKY_RAMP_TICKS).clamp(0.0, 1.0);
    let fall = ((t - (DUSK_TICK as f32 - half)) / SKY_RAMP_TICKS).clamp(0.0, 1.0);
    (rise - fall).clamp(0.0, 1.0)
}

/// Background color for a given tick-of-day.
pub fn sky_color(time: u32) -> Color {
    let d = daylight(time);
    // Peaks (= 1) exactly mid-transition, 0 at full day/night.
    let warm = d * (1.0 - d) * 4.0 * SKY_WARM_MAX;
    let mix = |night: f32, day: f32, w: f32| (night + (day - night) * d) * (1.0 - warm) + w * warm;
    Color::new(
        mix(SKY_NIGHT.0, SKY_DAY.0, SKY_WARM.0),
        mix(SKY_NIGHT.1, SKY_DAY.1, SKY_WARM.1),
        mix(SKY_NIGHT.2, SKY_DAY.2, SKY_WARM.2),
        1.0,
    )
}

// ---- Tile colors ------------------------------------------------------------------

/// Base color per [`TileId`], indexed by discriminant. Order must match the
/// `TileId` declaration (Air ... TreeTrunk).
const TILE_COLORS: [(u8, u8, u8); TileId::COUNT] = [
    (0, 0, 0),       // Air (never drawn)
    (139, 96, 60),   // Dirt
    (128, 128, 130), // Stone
    (66, 160, 66),   // Grass (green cap over dirt)
    (212, 200, 142), // Sand
    (158, 90, 74),   // Clay
    (170, 124, 86),  // WoodPlank
    (196, 116, 62),  // CopperOre
    (180, 152, 138), // IronOre
    (215, 222, 230), // SilverOre
    (232, 196, 66),  // GoldOre
    (186, 60, 42),   // Hellstone
    (64, 44, 94),    // Obsidian
    (96, 88, 88),    // Ash
    (110, 112, 120), // StoneBrick
    (158, 76, 54),   // EmberBrick
    (255, 200, 90),  // Torch (flame)
    (186, 144, 92),  // Platform
    (132, 94, 58),   // Door
    (168, 116, 62),  // Chest
    (158, 110, 72),  // Workbench
    (98, 98, 110),   // Furnace
    (76, 76, 86),    // Anvil
    (150, 64, 52),   // InfernalForge
    (130, 48, 132),  // RitualAltar
    (158, 110, 72),  // Table
    (158, 110, 72),  // Chair
    (188, 70, 82),   // Bed
    (176, 126, 84),  // Pot
    (236, 70, 100),  // LifeCrystal
    (235, 235, 240), // Cobweb
    (104, 176, 70),  // Sapling
    (118, 86, 56),   // TreeTrunk
];

/// Base color per [`WallId`]; drawn dimmed behind the foreground layer.
const WALL_COLORS: [(u8, u8, u8); WallId::COUNT] = [
    (0, 0, 0),     // Air (never drawn)
    (92, 64, 44),  // Dirt wall
    (72, 72, 78),  // Stone wall
    (116, 86, 60), // Wood wall
];

/// Walls render at this brightness so they read as background.
const WALL_DIM: f32 = 0.55;

const LEAF_COLOR: (u8, u8, u8) = (52, 132, 58);
const WATER_BODY: Color = Color::new(0.16, 0.36, 0.86, 0.55);
const WATER_SURFACE: Color = Color::new(0.55, 0.75, 1.0, 0.80);
const LAVA_BODY: Color = Color::new(0.96, 0.43, 0.12, 0.85);
const LAVA_SURFACE: Color = Color::new(1.0, 0.85, 0.35, 0.95);

/// Deterministic per-position brightness in 0.90..=1.0 — cheap integer hash,
/// so terrain gets subtle texture without any allocation.
fn brightness(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 16;
    0.90 + (h & 0xFF) as f32 / 255.0 * 0.10
}

fn tint(c: (u8, u8, u8), f: f32) -> Color {
    Color::new(
        c.0 as f32 / 255.0 * f,
        c.1 as f32 / 255.0 * f,
        c.2 as f32 / 255.0 * f,
        1.0,
    )
}

// ---- World drawing -----------------------------------------------------------------

/// Draws every tile (walls, foreground, liquids) intersecting the camera
/// view. One cell of padding so tree canopies spilling over cell bounds
/// don't pop at the screen edge.
pub fn draw_world(view: &WorldView, cam: &Camera) {
    let world = view.world();
    let tl = cam.top_left();
    let x0 = ((tl.x / TILE_SIZE).floor() as i64 - 1).max(0) as u32;
    let y0 = ((tl.y / TILE_SIZE).floor() as i64 - 1).max(0) as u32;
    let x1 = (((tl.x + screen_width()) / TILE_SIZE).ceil() as i64 + 1)
        .clamp(0, world.width as i64 - 1) as u32;
    let y1 = (((tl.y + screen_height()) / TILE_SIZE).ceil() as i64 + 1)
        .clamp(0, world.height as i64 - 1) as u32;

    for y in y0..=y1 {
        for x in x0..=x1 {
            let t = world.tile(x, y);
            let px = x as f32 * TILE_SIZE - tl.x;
            let py = y as f32 * TILE_SIZE - tl.y;
            draw_cell(world, x, y, t, px, py);
        }
    }
}

fn draw_cell(world: &World, x: u32, y: u32, t: Tile, px: f32, py: f32) {
    let b = brightness(x, y);
    let s = TILE_SIZE;

    // Background wall, where the foreground doesn't fully cover it.
    if t.wall != WallId::Air && !t.is_solid() {
        draw_rectangle(
            px,
            py,
            s,
            s,
            tint(WALL_COLORS[t.wall as usize], b * WALL_DIM),
        );
    }

    let base = tint(TILE_COLORS[t.id as usize], b);
    match t.id {
        TileId::Air => {}
        TileId::Grass => {
            // Dirt body with a grass cap.
            draw_rectangle(px, py, s, s, tint(TILE_COLORS[TileId::Dirt as usize], b));
            draw_rectangle(px, py, s, s * 0.3, base);
            // Mushroom forage plant (state::GRASS_MUSHROOM): stem + cap
            // spilling above the cell, like tree canopies do.
            if state::variant(t.state) == state::GRASS_MUSHROOM {
                draw_rectangle(
                    px + s * 0.44,
                    py - s * 0.30,
                    s * 0.12,
                    s * 0.32,
                    tint((226, 220, 200), b),
                );
                draw_circle(
                    px + s * 0.5,
                    py - s * 0.34,
                    s * 0.24,
                    tint((196, 62, 58), b),
                );
            }
        }
        TileId::Torch => {
            draw_rectangle(
                px + s * 0.45,
                py + s * 0.35,
                s * 0.12,
                s * 0.55,
                tint((110, 80, 50), b),
            );
            draw_circle(px + s * 0.5, py + s * 0.28, s * 0.22, base);
            draw_circle(
                px + s * 0.5,
                py + s * 0.28,
                s * 1.1,
                Color::new(1.0, 0.8, 0.4, 0.10),
            );
        }
        TileId::Platform => {
            draw_rectangle(px, py, s, s * 0.35, base);
        }
        TileId::Door => {
            if t.state & state::DOOR_OPEN != 0 {
                // Swung against the jamb on the side away from whoever
                // opened it (state::DOOR_OPEN_LEFT).
                let jamb_x = if t.state & state::DOOR_OPEN_LEFT != 0 {
                    px
                } else {
                    px + s * 0.8
                };
                draw_rectangle(jamb_x, py, s * 0.2, s, base);
            } else {
                draw_rectangle(px + s * 0.15, py, s * 0.7, s, base);
            }
        }
        TileId::TreeTrunk => {
            if t.state & 0x7 == state::TREE_SEGMENT_TOP {
                draw_rectangle(px + s * 0.35, py + s * 0.5, s * 0.3, s * 0.5, base);
                draw_circle(
                    px + s * 0.5,
                    py + s * 0.35,
                    s * 0.85,
                    tint(LEAF_COLOR, b * 0.85),
                );
                draw_circle(px + s * 0.5, py + s * 0.15, s * 0.6, tint(LEAF_COLOR, b));
            } else {
                draw_rectangle(px + s * 0.3, py, s * 0.4, s, base);
            }
        }
        TileId::Sapling => {
            draw_rectangle(
                px + s * 0.45,
                py + s * 0.4,
                s * 0.1,
                s * 0.6,
                tint((110, 80, 50), b),
            );
            draw_circle(px + s * 0.5, py + s * 0.35, s * 0.25, base);
        }
        TileId::Cobweb => {
            let c = Color::new(0.92, 0.92, 0.94, 0.6);
            draw_line(px, py, px + s, py + s, 1.0, c);
            draw_line(px + s, py, px, py + s, 1.0, c);
        }
        TileId::Pot => {
            draw_circle(px + s * 0.5, py + s * 0.6, s * 0.38, base);
            draw_rectangle(
                px + s * 0.3,
                py + s * 0.1,
                s * 0.4,
                s * 0.18,
                tint((130, 90, 60), b),
            );
        }
        TileId::LifeCrystal => {
            draw_rectangle(px + s * 0.25, py + s * 0.25, s * 0.5, s * 0.5, base);
            draw_circle(
                px + s * 0.5,
                py + s * 0.5,
                s * 0.65,
                Color::new(0.95, 0.3, 0.4, 0.12),
            );
        }
        id if id.data().furniture => {
            // Generic furniture cell: inset so multi-tile objects read as
            // one piece against the background; a darker base line grounds
            // the bottom row of the footprint.
            draw_rectangle(px + 1.0, py + 1.0, s - 2.0, s - 2.0, base);
            let (_, fh) = id.data().size;
            if state::part_y(t.state) + 1 == fh {
                draw_rectangle(
                    px + 1.0,
                    py + s - 3.0,
                    s - 2.0,
                    2.0,
                    tint(TILE_COLORS[t.id as usize], b * 0.6),
                );
            }
        }
        _ => draw_rectangle(px, py, s, s, base),
    }

    // Liquid overlay: fills from the cell bottom, semi-transparent, with a
    // lighter surface line where the cell above holds no liquid.
    if let Some(kind) = t.liquid.kind() {
        let level = t.liquid.level() as f32 / LIQUID_MAX_LEVEL as f32;
        let h = level * s;
        let (body, surface) = match kind {
            LiquidKind::Water => (WATER_BODY, WATER_SURFACE),
            LiquidKind::Lava => (LAVA_BODY, LAVA_SURFACE),
        };
        draw_rectangle(px, py + s - h, s, h, body);
        if world.liquid(x as i32, y as i32 - 1).is_none() {
            draw_rectangle(px, py + s - h, s, 1.5, surface);
        }
    }
}

// ---- Players ---------------------------------------------------------------------

const SHIRT_SELF: Color = Color::new(0.30, 0.52, 0.82, 1.0);
const SHIRT_OTHER: Color = Color::new(0.80, 0.40, 0.32, 1.0);
const PANTS: Color = Color::new(0.24, 0.24, 0.36, 1.0);
const SKIN: Color = Color::new(0.94, 0.78, 0.62, 1.0);
const EYE: Color = Color::new(0.08, 0.08, 0.10, 1.0);
const NAME_SIZE: f32 = 18.0;

/// Everything needed to draw one player sprite.
pub struct PlayerDraw<'a> {
    /// Screen px of the AABB top-left.
    pub pos: Vec2,
    /// World-space x (tiles), drives the 2-frame walk cycle.
    pub world_x: f32,
    pub vel_x: f32,
    pub facing: i8,
    pub anim: u8,
    pub name: &'a str,
    pub is_self: bool,
    pub held_item: Option<ItemId>,
}

/// 20×44 px capsule-ish figure: head circle, shirt torso, two pants legs
/// with a 2-frame walk wobble, eye dot flipped by facing, name label above.
pub fn draw_player(p: &PlayerDraw) {
    let w = PLAYER_WIDTH * TILE_SIZE; // 20
    let h = PLAYER_HEIGHT * TILE_SIZE; // 44
    let (x, y) = (p.pos.x, p.pos.y);
    let shirt = if p.is_self { SHIRT_SELF } else { SHIRT_OTHER };
    let grounded = p.anim & anim::GROUNDED != 0;
    let walking = grounded && p.vel_x.abs() > 0.5;
    // Alternates every half tile of travel.
    let phase = ((p.world_x * 2.0).floor() as i64) & 1 == 0;

    // Legs (bottom 12 px), wobbling while walking, trailing while airborne.
    let leg_h = 12.0;
    let leg_y = y + h - leg_h;
    let (off_l, off_r) = if walking {
        if phase {
            (-2.0, 2.0)
        } else {
            (2.0, -2.0)
        }
    } else if !grounded {
        (-1.5, 1.5)
    } else {
        (0.0, 0.0)
    };
    draw_rectangle(x + 3.0 + off_l, leg_y, 5.5, leg_h, PANTS);
    draw_rectangle(x + w - 8.5 + off_r, leg_y, 5.5, leg_h, PANTS);

    // Torso and head.
    draw_rectangle(x + 2.0, y + 12.0, w - 4.0, h - 12.0 - leg_h, shirt);
    draw_circle(x + w * 0.5, y + 7.0, 7.0, SKIN);
    let eye_dx = p.facing as f32 * 3.0;
    draw_circle(x + w * 0.5 + eye_dx, y + 6.0, 1.6, EYE);

    // Held item, as a small swatch in the leading hand.
    if let Some(item) = p.held_item {
        let hx = if p.facing >= 0 { x + w - 1.0 } else { x - 5.0 };
        draw_rectangle(hx, y + 18.0, 6.0, 6.0, item_color(item));
    }

    // Name label, centered above the head.
    let dims = measure_text(p.name, None, NAME_SIZE as u16, 1.0);
    let nx = x + w * 0.5 - dims.width * 0.5;
    let ny = y - 8.0;
    draw_text(
        p.name,
        nx + 1.0,
        ny + 1.0,
        NAME_SIZE,
        Color::new(0.0, 0.0, 0.0, 0.6),
    );
    draw_text(p.name, nx, ny, NAME_SIZE, WHITE);
}

/// Base color of a tile at full brightness — for entities that mirror tiles
/// (falling sand).
pub fn tile_color(id: TileId) -> Color {
    tint(TILE_COLORS[id as usize], 1.0)
}

/// Stable distinctive color for an item swatch — held items, hotbar slots,
/// and dropped-item entities all share it (a real sprite atlas later).
pub fn item_color(item: ItemId) -> Color {
    let n = item as u32;
    let mut h = n.wrapping_mul(0x9E37_79B9);
    h ^= h >> 15;
    Color::new(
        0.35 + (h & 0xFF) as f32 / 255.0 * 0.6,
        0.35 + ((h >> 8) & 0xFF) as f32 / 255.0 * 0.6,
        0.35 + ((h >> 16) & 0xFF) as f32 / 255.0 * 0.6,
        1.0,
    )
}
