//! All drawing: camera, sky (gradient, sun/moon, stars), the lit tile world,
//! and player sprites.
//!
//! Primitive shapes only (rects/circles/lines) — a texture atlas comes in a
//! later PR. Only camera-visible tiles are drawn, and nothing here allocates
//! per frame (the darkness/sky meshes reuse their buffers). Tile colors live
//! in one table indexed by `TileId`; a cheap per-position hash varies
//! brightness so terrain isn't flat. Light levels come from
//! [`crate::light::LightEngine`] and darken the world through a per-corner
//! shaded overlay mesh, so lighting reads as a smooth gradient instead of
//! per-tile blocks.

use macroquad::models::{draw_mesh, Mesh, Vertex};
use macroquad::prelude::*;

use ferraria_shared::items::ItemId;
use ferraria_shared::physics::{PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::anim;
use ferraria_shared::tiles::{state, LiquidKind, Tile, TileId, WallId, LIQUID_MAX_LEVEL};
use ferraria_shared::world::{World, DAWN_TICK, DAY_TICKS, DUSK_TICK};
use ferraria_shared::TILE_SIZE;

use crate::light::{daylight, LightEngine};
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

    /// Inclusive tile rect (x0, y0, x1, y1) covering the screen plus one
    /// cell of padding, clamped to the world. Shared by world drawing and
    /// the light engine so they agree on "visible".
    pub fn visible_tiles(&self, world: &World) -> (u32, u32, u32, u32) {
        let tl = self.top_left();
        let x0 = ((tl.x / TILE_SIZE).floor() as i64 - 1).max(0) as u32;
        let y0 = ((tl.y / TILE_SIZE).floor() as i64 - 1).max(0) as u32;
        let x1 = (((tl.x + screen_width()) / TILE_SIZE).ceil() as i64 + 1)
            .clamp(0, world.width as i64 - 1) as u32;
        let y1 = (((tl.y + screen_height()) / TILE_SIZE).ceil() as i64 + 1)
            .clamp(0, world.height as i64 - 1) as u32;
        (x0, y0, x1, y1)
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

// ---- Per-vertex-colored quads -----------------------------------------------------

/// Quads per mesh flush: 6 indices each, kept under macroquad's default
/// 5000-index / 10000-vertex draw-call capacity.
const BATCH_MAX_QUADS: usize = 800;

/// Batches axis-aligned quads with per-vertex colors into one reusable mesh
/// (`draw_rectangle` has no per-vertex colors). Used for the smooth lighting
/// overlay, the sky gradient, and the night vignette.
pub struct QuadBatch {
    mesh: Mesh,
}

impl QuadBatch {
    pub fn new() -> QuadBatch {
        QuadBatch {
            mesh: Mesh {
                vertices: Vec::new(),
                indices: Vec::new(),
                texture: None,
            },
        }
    }

    /// Queues one quad; colors are [top-left, top-right, bottom-right,
    /// bottom-left].
    pub fn quad(&mut self, x: f32, y: f32, w: f32, h: f32, colors: [Color; 4]) {
        if self.mesh.vertices.len() / 4 >= BATCH_MAX_QUADS {
            self.flush();
        }
        let base = self.mesh.vertices.len() as u16;
        let v = |px: f32, py: f32, c: Color| Vertex::new(px, py, 0.0, 0.0, 0.0, c);
        self.mesh.vertices.push(v(x, y, colors[0]));
        self.mesh.vertices.push(v(x + w, y, colors[1]));
        self.mesh.vertices.push(v(x + w, y + h, colors[2]));
        self.mesh.vertices.push(v(x, y + h, colors[3]));
        self.mesh
            .indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    /// Draws everything queued and resets the buffers (capacity kept).
    pub fn flush(&mut self) {
        if !self.mesh.vertices.is_empty() {
            draw_mesh(&self.mesh);
            self.mesh.vertices.clear();
            self.mesh.indices.clear();
        }
    }
}

impl Default for QuadBatch {
    fn default() -> Self {
        QuadBatch::new()
    }
}

// ---- Sky ------------------------------------------------------------------------

const SKY_DAY: (f32, f32, f32) = (0.45, 0.70, 0.92);
const SKY_NIGHT: (f32, f32, f32) = (0.03, 0.04, 0.11);
/// Warm dawn/dusk tint, blended in around the transitions.
const SKY_WARM: (f32, f32, f32) = (0.95, 0.55, 0.30);
const SKY_WARM_MAX: f32 = 0.40;

/// The backdrop is a vertical gradient: zenith darker than `sky_color`,
/// horizon lighter (and warmer around dawn/dusk).
const SKY_ZENITH_MUL: f32 = 0.72;
const SKY_HORIZON_LIFT: f32 = 0.18;

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

/// Deterministic star field: count, and a hash giving each star a fixed
/// screen position, size, and twinkle phase/speed.
const STAR_COUNT: u32 = 130;

fn star_hash(i: u32) -> u32 {
    let mut h = i.wrapping_mul(0x9E37_79B9) ^ 0x5BD1_E995;
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^ (h >> 16)
}

/// Sun/moon disc radius and arc shape, as fractions of the screen.
const ORB_RADIUS_PX: f32 = 26.0;
const ARC_TOP: f32 = 0.16; // arc apex, fraction of screen height
const ARC_BOTTOM: f32 = 0.85; // arc ends, fraction of screen height

/// Screen position along the day/night arc for progress `p` in 0..1.
fn arc_pos(p: f32) -> Vec2 {
    let x = (-0.06 + 1.12 * p) * screen_width();
    let lift = (p * std::f32::consts::PI).sin();
    let y = (ARC_BOTTOM - (ARC_BOTTOM - ARC_TOP) * lift) * screen_height();
    vec2(x, y)
}

/// Full sky backdrop: vertical gradient, stars at night (twinkling), and the
/// sun or moon arcing across. Screen-space; draw before the world.
pub fn draw_sky(time: u32, now: f64, batch: &mut QuadBatch) {
    let base = sky_color(time);
    let zenith = Color::new(
        base.r * SKY_ZENITH_MUL,
        base.g * SKY_ZENITH_MUL,
        base.b * SKY_ZENITH_MUL,
        1.0,
    );
    let horizon = Color::new(
        (base.r + SKY_HORIZON_LIFT).min(1.0),
        (base.g + SKY_HORIZON_LIFT).min(1.0),
        (base.b + SKY_HORIZON_LIFT).min(1.0),
        1.0,
    );
    let (w, h) = (screen_width(), screen_height());
    batch.quad(0.0, 0.0, w, h, [zenith, zenith, horizon, horizon]);
    batch.flush();

    // Stars fade in with the night; deterministic positions, sine twinkle.
    let night = 1.0 - daylight(time);
    if night > 0.0 {
        for i in 0..STAR_COUNT {
            let hsh = star_hash(i);
            let sx = (hsh & 0x3FF) as f32 / 1023.0 * w;
            let sy = ((hsh >> 10) & 0x3FF) as f32 / 1023.0 * h * 0.85;
            let size = 1.0 + ((hsh >> 20) & 0x3) as f32 * 0.5;
            let phase = ((hsh >> 22) & 0xFF) as f32 / 255.0 * std::f32::consts::TAU;
            let speed = 0.8 + ((hsh >> 27) & 0x7) as f32 * 0.35;
            let twinkle = 0.55 + 0.45 * (now as f32 * speed + phase).sin();
            let a = night * twinkle;
            draw_rectangle(sx, sy, size, size, Color::new(0.95, 0.95, 1.0, a));
        }
    }

    if (DAWN_TICK..DUSK_TICK).contains(&time) {
        // Sun: progress 0..1 across the day.
        let p = (time - DAWN_TICK) as f32 / (DUSK_TICK - DAWN_TICK) as f32;
        let pos = arc_pos(p);
        draw_circle(
            pos.x,
            pos.y,
            ORB_RADIUS_PX * 2.2,
            Color::new(1.0, 0.85, 0.5, 0.18),
        );
        draw_circle(
            pos.x,
            pos.y,
            ORB_RADIUS_PX * 1.35,
            Color::new(1.0, 0.9, 0.55, 0.35),
        );
        draw_circle(
            pos.x,
            pos.y,
            ORB_RADIUS_PX,
            Color::new(1.0, 0.93, 0.65, 1.0),
        );
    } else {
        // Moon: progress 0..1 across the night (which wraps midnight).
        let since_dusk = (time + DAY_TICKS - DUSK_TICK) % DAY_TICKS;
        let night_len = DAY_TICKS - (DUSK_TICK - DAWN_TICK);
        let p = since_dusk as f32 / night_len as f32;
        let pos = arc_pos(p);
        let r = ORB_RADIUS_PX * 0.8;
        draw_circle(pos.x, pos.y, r * 1.6, Color::new(0.8, 0.85, 1.0, 0.10));
        draw_circle(pos.x, pos.y, r, Color::new(0.88, 0.90, 0.96, 1.0));
        // Crescent shadow: offset disc in the zenith color.
        draw_circle(pos.x + r * 0.45, pos.y - r * 0.2, r * 0.85, zenith);
    }
}

/// Subtle screen-edge vignette, drawn over the world at night while the
/// player is on the surface. Four gradient quads — effectively free.
const VIGNETTE_MAX_ALPHA: f32 = 0.35;
const VIGNETTE_BAND_FRAC: f32 = 0.16;

pub fn draw_vignette(night: f32, batch: &mut QuadBatch) {
    let a = VIGNETTE_MAX_ALPHA * night;
    if a <= 0.0 {
        return;
    }
    let (w, h) = (screen_width(), screen_height());
    let band = w.min(h) * VIGNETTE_BAND_FRAC;
    let edge = Color::new(0.0, 0.0, 0.0, a);
    let clear = Color::new(0.0, 0.0, 0.0, 0.0);
    batch.quad(0.0, 0.0, w, band, [edge, edge, clear, clear]);
    batch.quad(0.0, h - band, w, band, [clear, clear, edge, edge]);
    batch.quad(0.0, 0.0, band, h, [edge, clear, clear, edge]);
    batch.quad(w - band, 0.0, band, h, [clear, edge, edge, clear]);
    batch.flush();
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

/// Below this brightness on all 4 corners a cell is pitch black: skip its
/// content and let the darkness overlay paint it out.
const DARK_SKIP: f32 = 1.0 / 255.0;

/// Draws every tile (walls, foreground, liquids) intersecting the camera
/// view, then the smooth-lighting darkness overlay. One cell of padding so
/// tree canopies spilling over cell bounds don't pop at the screen edge.
///
/// Lighting: pass 1 draws cell content at full color; pass 2 lays a black
/// quad over each cell whose per-corner alpha is `1 − light/32` (corners
/// sample the 4 surrounding tiles via [`LightEngine::corner_light`]), which
/// both multiplies the colors by the light level and blends it across tile
/// boundaries.
///
/// `corners` is a caller-owned reusable buffer holding the visible rect's
/// corner-brightness grid, computed once and shared by both passes: every
/// interior corner adjoins 4 cells and both passes read all 4 corners of
/// each cell, so computing corners per cell would redo each one up to 8
/// times (~123k clamped light reads per frame at 1080p instead of ~4k).
pub fn draw_world(
    view: &WorldView,
    cam: &Camera,
    light: &LightEngine,
    batch: &mut QuadBatch,
    corners: &mut Vec<f32>,
) {
    let world = view.world();
    let tl = cam.top_left();
    let (x0, y0, x1, y1) = cam.visible_tiles(world);

    // Corner grid: one more column/row of corners than cells.
    let cols = (x1 - x0) as usize + 2;
    let rows = (y1 - y0) as usize + 2;
    corners.clear();
    corners.reserve(cols * rows);
    for cy in y0 as i64..y0 as i64 + rows as i64 {
        for cx in x0 as i64..x0 as i64 + cols as i64 {
            corners.push(light.corner_light(cx, cy));
        }
    }
    // Brightness (0–1) of cell (x, y)'s corners: [TL, TR, BR, BL].
    let cell_corners = |x: u32, y: u32| -> [f32; 4] {
        let i = (y - y0) as usize * cols + (x - x0) as usize;
        [
            corners[i],
            corners[i + 1],
            corners[i + cols + 1],
            corners[i + cols],
        ]
    };

    for y in y0..=y1 {
        for x in x0..=x1 {
            // Fully dark cell: the overlay covers it; don't waste fill.
            if cell_corners(x, y).iter().all(|&c| c <= DARK_SKIP) {
                continue;
            }
            let t = world.tile(x, y);
            let px = x as f32 * TILE_SIZE - tl.x;
            let py = y as f32 * TILE_SIZE - tl.y;
            draw_cell(world, x, y, t, px, py);
        }
    }

    // Darkness overlay (after all content: canopies/glows spill over cells).
    for y in y0..=y1 {
        for x in x0..=x1 {
            let c = cell_corners(x, y);
            if c.iter().all(|&l| l >= 1.0 - DARK_SKIP) {
                continue; // fully lit
            }
            let px = x as f32 * TILE_SIZE - tl.x;
            let py = y as f32 * TILE_SIZE - tl.y;
            let shade = |l: f32| Color::new(0.0, 0.0, 0.0, 1.0 - l);
            batch.quad(
                px,
                py,
                TILE_SIZE,
                TILE_SIZE,
                [shade(c[0]), shade(c[1]), shade(c[2]), shade(c[3])],
            );
        }
    }
    batch.flush();
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

/// Head-circle center, px below the sprite's AABB top. The Mining Helmet's
/// light source anchors to the tile containing this point (§10 "at player
/// head").
pub const HEAD_CENTER_PX: f32 = 7.0;

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
    /// Live §4.1 use animation: (progress 0..1, is_bow) from
    /// [`crate::interact::Interact::swing`]. Melee items sweep an arc, bows
    /// draw back. `None` between uses (and always for remote players — the
    /// wire doesn't carry swing timing).
    pub swing: Option<(f32, bool)>,
    /// 0–1 brightness at the player's center tile
    /// ([`LightEngine::brightness_at`]); multiplies the sprite colors.
    pub light: f32,
}

/// Melee swing arc, degrees from horizontal-forward (negative = above):
/// the item sweeps from overhead down past level over the use time.
const SWING_START_DEG: f32 = -110.0;
const SWING_END_DEG: f32 = 40.0;
/// Swing pivot ("shoulder") px below the AABB top, and blade size.
const SWING_PIVOT_PX: f32 = 16.0;
const SWING_LEN_PX: f32 = 17.0;
const SWING_THICK_PX: f32 = 4.0;
/// Bow draw-back: how far the bow pulls toward the body at full draw.
const BOW_PULL_PX: f32 = 4.0;

/// Multiplies a color's RGB by a light factor (alpha untouched).
pub fn lit_color(c: Color, l: f32) -> Color {
    Color::new(c.r * l, c.g * l, c.b * l, c.a)
}

/// 20×44 px capsule-ish figure: head circle, shirt torso, two pants legs
/// with a 2-frame walk wobble, eye dot flipped by facing, name label above.
/// Tinted by the light level at the player (the name label stays readable).
pub fn draw_player(p: &PlayerDraw) {
    let w = PLAYER_WIDTH * TILE_SIZE; // 20
    let h = PLAYER_HEIGHT * TILE_SIZE; // 44
    let (x, y) = (p.pos.x, p.pos.y);
    let l = p.light.clamp(0.0, 1.0);
    let shirt = lit_color(if p.is_self { SHIRT_SELF } else { SHIRT_OTHER }, l);
    let pants = lit_color(PANTS, l);
    let skin = lit_color(SKIN, l);
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
    draw_rectangle(x + 3.0 + off_l, leg_y, 5.5, leg_h, pants);
    draw_rectangle(x + w - 8.5 + off_r, leg_y, 5.5, leg_h, pants);

    // Torso and head.
    draw_rectangle(x + 2.0, y + 12.0, w - 4.0, h - 12.0 - leg_h, shirt);
    draw_circle(x + w * 0.5, y + HEAD_CENTER_PX, 7.0, skin);
    let eye_dx = p.facing as f32 * 3.0;
    draw_circle(x + w * 0.5 + eye_dx, y + 6.0, 1.6, EYE);

    // Held item: a small swatch in the leading hand, or its live §4.1 use
    // animation — a rotation arc for melee swings, a draw-back for bows.
    if let Some(item) = p.held_item {
        let color = lit_color(item_color(item), l);
        let dir = if p.facing >= 0 { 1.0 } else { -1.0 };
        match p.swing {
            Some((t, false)) => {
                // Melee: the item sweeps an arc around the shoulder.
                let deg = SWING_START_DEG + (SWING_END_DEG - SWING_START_DEG) * t;
                let a = deg.to_radians();
                // Mirror across the vertical axis when facing left.
                let rot = if dir >= 0.0 {
                    a
                } else {
                    std::f32::consts::PI - a
                };
                draw_rectangle_ex(
                    x + w * 0.5,
                    y + SWING_PIVOT_PX,
                    SWING_LEN_PX,
                    SWING_THICK_PX,
                    DrawRectangleParams {
                        offset: vec2(0.0, 0.5), // pivot at the hand end
                        rotation: rot,
                        color,
                    },
                );
            }
            Some((t, true)) => {
                // Bow: the swatch pulls back toward the body as it draws,
                // with a taut "string" line at the leading edge.
                let pull = dir * BOW_PULL_PX * t;
                let hx = if p.facing >= 0 { x + w - 1.0 } else { x - 5.0 } - pull;
                draw_rectangle(hx, y + 18.0, 6.0, 6.0, color);
                draw_line(hx + 3.0, y + 16.0, hx + 3.0, y + 26.0, 1.0, color);
            }
            None => {
                let hx = if p.facing >= 0 { x + w - 1.0 } else { x - 5.0 };
                draw_rectangle(hx, y + 18.0, 6.0, 6.0, color);
            }
        }
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

/// Stable distinctive color for an item swatch/glyph (a real sprite atlas
/// later). Held items, hotbar slots, dropped-item entities, and the
/// inventory UI all share it so an item looks the same everywhere.
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
