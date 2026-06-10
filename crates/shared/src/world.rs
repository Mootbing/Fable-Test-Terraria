//! The world model: tile grid, spawn, time, world flags, chest contents,
//! and chunk encoding (64×64 tiles, lz4-compressed) for the wire.
//!
//! Coordinates: `x` grows right, `y` grows down, row 0 is the top of the
//! world (DESIGN §1). Positions in tile units.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::items::InvSlot;
use crate::tiles::{state, Liquid, Tile, TileId};

/// Default world size (§0).
pub const WORLD_WIDTH: u32 = 4200;
pub const WORLD_HEIGHT: u32 = 1200;

/// Chunks are 64×64 tiles; the server streams the 5×3 neighborhood around
/// each player (see the `CHUNK_SUB_*` constants).
pub const CHUNK_SIZE: u32 = 64;

/// Per-player chunk subscription window: ±2 chunks horizontally and ±1
/// vertically (5×3 — wider than tall because screens are). A player
/// unsubscribes from a chunk only once it falls [`CHUNK_SUB_HYSTERESIS`]
/// chunks outside this window, so hovering on a boundary doesn't thrash.
pub const CHUNK_SUB_RADIUS_X: u32 = 2;
pub const CHUNK_SUB_RADIUS_Y: u32 = 1;
pub const CHUNK_SUB_HYSTERESIS: u32 = 1;

/// Chests have 40 slots (§2 tile 19).
pub const CHEST_SLOTS: usize = 40;

// ---- Time (§9): 1 in-game minute = 1 real second = 60 ticks ----------------

/// Ticks per full day/night cycle (24 real minutes).
pub const DAY_TICKS: u32 = 86_400;
/// 4:30 AM — dawn.
pub const DAWN_TICK: u32 = 16_200;
/// 7:30 PM — dusk.
pub const DUSK_TICK: u32 = 70_200;
/// New worlds start at 8:15 AM.
pub const NEW_WORLD_TIME: u32 = 29_700;
/// The Watcher's pre-spawn warning leads the spawn by 81 real seconds (§6.2).
pub const WATCHER_WARNING_LEAD_TICKS: u32 = 81 * 60;

/// Day is 4:30 AM – 7:30 PM (`time` is the tick-of-day, `0..DAY_TICKS`).
pub fn is_day(time: u32) -> bool {
    (DAWN_TICK..DUSK_TICK).contains(&time)
}

/// World-level progress flags (bosses defeated etc.), shared with clients.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldFlags {
    pub slime_monarch_defeated: bool,
    pub watcher_defeated: bool,
    pub bone_warden_defeated: bool,
}

/// The authoritative world state (server) / mirrored subset (client).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct World {
    pub width: u32,
    pub height: u32,
    /// Row-major, `width * height` cells, index = `y * width + x`.
    pub tiles: Vec<Tile>,
    /// World spawn point in tile coords (player feet stand at `spawn.1`).
    pub spawn: (u32, u32),
    /// Tick of day, `0..DAY_TICKS` (see [`is_day`]).
    pub time: u32,
    /// Completed day count since world creation.
    pub day: u32,
    pub flags: WorldFlags,
    /// Chest contents keyed by the chest's origin (top-left) tile coord.
    /// `BTreeMap` for deterministic serialization. Each entry has
    /// [`CHEST_SLOTS`] slots.
    pub chests: BTreeMap<(u32, u32), Vec<Option<InvSlot>>>,
}

impl World {
    /// An all-air world (generation fills it in).
    pub fn new(width: u32, height: u32) -> World {
        World {
            width,
            height,
            tiles: vec![Tile::AIR; width as usize * height as usize],
            spawn: (width / 2, 0),
            time: NEW_WORLD_TIME,
            day: 0,
            flags: WorldFlags::default(),
            chests: BTreeMap::new(),
        }
    }

    #[inline]
    pub fn in_bounds(&self, x: u32, y: u32) -> bool {
        x < self.width && y < self.height
    }

    #[inline]
    fn idx(&self, x: u32, y: u32) -> usize {
        y as usize * self.width as usize + x as usize
    }

    /// The cell at (x, y); air when out of bounds.
    #[inline]
    pub fn tile(&self, x: u32, y: u32) -> Tile {
        if self.in_bounds(x, y) {
            self.tiles[self.idx(x, y)]
        } else {
            Tile::AIR
        }
    }

    /// Writes a cell; silently ignores out-of-bounds (callers validate).
    #[inline]
    pub fn set_tile(&mut self, x: u32, y: u32, tile: Tile) {
        if self.in_bounds(x, y) {
            let i = self.idx(x, y);
            self.tiles[i] = tile;
        }
    }

    /// Fully solid at signed coords. Out-of-bounds counts as solid so the
    /// world border behaves like a wall for physics.
    #[inline]
    pub fn is_solid(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 {
            return true;
        }
        let (x, y) = (x as u32, y as u32);
        if !self.in_bounds(x, y) {
            return true;
        }
        self.tiles[self.idx(x, y)].is_solid()
    }

    /// Platform (solid-from-above) at signed coords.
    #[inline]
    pub fn is_platform(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 {
            return false;
        }
        self.in_bounds(x as u32, y as u32) && self.tile(x as u32, y as u32).is_platform()
    }

    /// Liquid at signed coords ([`Liquid::NONE`] out of bounds).
    #[inline]
    pub fn liquid(&self, x: i32, y: i32) -> Liquid {
        if x < 0 || y < 0 {
            return Liquid::NONE;
        }
        self.tile(x as u32, y as u32).liquid
    }

    /// No foreground tile here — free for tile placement. (Liquid and walls
    /// don't block placement.)
    #[inline]
    pub fn is_empty(&self, x: u32, y: u32) -> bool {
        self.in_bounds(x, y) && self.tile(x, y).id == TileId::Air
    }

    pub fn is_day(&self) -> bool {
        is_day(self.time)
    }

    /// Places a multi-tile object (furniture etc.) with its origin (top-left)
    /// at `(x, y)`, using the footprint from `TILE_DATA[id].size`. Every
    /// covered cell gets `id` with its frame byte set to
    /// [`state::part`]`(dx, dy)` so all parts agree on the object's layout;
    /// walls and liquid in the cells are preserved.
    ///
    /// Returns `false` (changing nothing) unless the whole footprint is in
    /// bounds and empty of foreground tiles. Callers enforce furniture-
    /// specific support rules (floor below, attach points, ...).
    pub fn place_multitile(&mut self, x: u32, y: u32, id: TileId) -> bool {
        let (w, h) = id.data().size;
        let (w, h) = (w as u32, h as u32);
        for dy in 0..h {
            for dx in 0..w {
                if !self.in_bounds(x + dx, y + dy) || !self.is_empty(x + dx, y + dy) {
                    return false;
                }
            }
        }
        for dy in 0..h {
            for dx in 0..w {
                let mut t = self.tile(x + dx, y + dy);
                t.id = id;
                // Keep the WALL_PLACED bit: it describes the cell's *wall*
                // (player-placed walls drop their item, §2), which placing
                // furniture over must not reclassify as natural.
                t.state = (t.state & state::WALL_PLACED) | state::part(dx as u8, dy as u8);
                self.set_tile(x + dx, y + dy, t);
            }
        }
        true
    }

    /// Origin (top-left) of the multi-tile object covering `(x, y)`, derived
    /// from the cell's frame byte. Returns `(x, y)` itself for 1×1 tiles —
    /// including ones whose state bits 0–4 carry a sprite variant instead of
    /// a part offset (`state::GRASS_MUSHROOM`, tree segment kinds, ...).
    pub fn multitile_origin(&self, x: u32, y: u32) -> (u32, u32) {
        let t = self.tile(x, y);
        if t.id.data().size == (1, 1) {
            return (x, y);
        }
        (
            x.saturating_sub(state::part_x(t.state) as u32),
            y.saturating_sub(state::part_y(t.state) as u32),
        )
    }

    /// Chunk-grid dimensions (edge chunks may hang past the world edge and
    /// are padded with air on encode).
    pub fn chunks_x(&self) -> u32 {
        self.width.div_ceil(CHUNK_SIZE)
    }

    pub fn chunks_y(&self) -> u32 {
        self.height.div_ceil(CHUNK_SIZE)
    }

    /// Encodes chunk (cx, cy): 64×64 raw 4-byte tiles, row-major,
    /// lz4-compressed with a length prefix. Out-of-world cells encode as air.
    pub fn encode_chunk(&self, cx: u32, cy: u32) -> Vec<u8> {
        let mut raw = Vec::with_capacity((CHUNK_SIZE * CHUNK_SIZE * 4) as usize);
        for dy in 0..CHUNK_SIZE {
            for dx in 0..CHUNK_SIZE {
                let t = self.tile(cx * CHUNK_SIZE + dx, cy * CHUNK_SIZE + dy);
                raw.extend_from_slice(&t.to_bytes());
            }
        }
        lz4_flex::compress_prepend_size(&raw)
    }

    /// Writes decoded chunk tiles back into the grid (client mirror).
    /// Cells outside the world are dropped.
    pub fn apply_chunk(&mut self, cx: u32, cy: u32, tiles: &[Tile]) {
        for (i, &t) in tiles.iter().enumerate() {
            let dx = i as u32 % CHUNK_SIZE;
            let dy = i as u32 / CHUNK_SIZE;
            self.set_tile(cx * CHUNK_SIZE + dx, cy * CHUNK_SIZE + dy, t);
        }
    }
}

/// Within mine/place reach (§8, [`crate::REACH`] = 6) of any cell of a
/// chest's 2×2 footprint, measured player center → cell center. One rule,
/// used by the server (open validation + walk-away lock release) and the
/// client (walk-away panel close) so they always agree.
pub fn chest_in_reach(center: (f32, f32), origin: (u32, u32)) -> bool {
    let (w, h) = TileId::Chest.data().size;
    for dy in 0..h as u32 {
        for dx in 0..w as u32 {
            let (cx, cy) = ((origin.0 + dx) as f32 + 0.5, (origin.1 + dy) as f32 + 0.5);
            let (ddx, ddy) = (cx - center.0, cy - center.1);
            if ddx * ddx + ddy * ddy <= crate::REACH * crate::REACH {
                return true;
            }
        }
    }
    false
}

/// Errors from [`decode_chunk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkDecodeError {
    /// lz4 decompression failed (corrupt frame or bad length prefix).
    Compression,
    /// Decompressed payload isn't exactly 64×64×4 bytes.
    WrongSize,
    /// A cell contained an invalid tile/wall/liquid byte.
    InvalidTile,
}

impl std::fmt::Display for ChunkDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            ChunkDecodeError::Compression => "chunk decompression failed",
            ChunkDecodeError::WrongSize => "chunk has wrong decompressed size",
            ChunkDecodeError::InvalidTile => "chunk contains an invalid tile",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for ChunkDecodeError {}

/// Decodes a chunk produced by [`World::encode_chunk`] into
/// `CHUNK_SIZE * CHUNK_SIZE` row-major tiles.
pub fn decode_chunk(bytes: &[u8]) -> Result<Vec<Tile>, ChunkDecodeError> {
    let raw =
        lz4_flex::decompress_size_prepended(bytes).map_err(|_| ChunkDecodeError::Compression)?;
    if raw.len() != (CHUNK_SIZE * CHUNK_SIZE * 4) as usize {
        return Err(ChunkDecodeError::WrongSize);
    }
    raw.chunks_exact(4)
        .map(|c| Tile::from_bytes([c[0], c[1], c[2], c[3]]).ok_or(ChunkDecodeError::InvalidTile))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Pcg32;
    use crate::tiles::{state, LiquidKind, WallId};

    #[test]
    fn tile_accessors_and_bounds() {
        let mut w = World::new(100, 50);
        assert!(w.in_bounds(99, 49));
        assert!(!w.in_bounds(100, 49));
        assert!(!w.in_bounds(99, 50));

        w.set_tile(5, 7, Tile::of(TileId::Stone));
        assert_eq!(w.tile(5, 7).id, TileId::Stone);
        assert!(w.is_solid(5, 7));
        assert!(!w.is_empty(5, 7));
        assert!(w.is_empty(6, 7));

        // Out of bounds: reads are air, writes are dropped, solidity is wall.
        assert_eq!(w.tile(1000, 0), Tile::AIR);
        w.set_tile(1000, 0, Tile::of(TileId::Stone)); // no panic
        assert!(w.is_solid(-1, 10));
        assert!(w.is_solid(0, 50));
        assert!(!w.is_platform(-1, -1));
        assert_eq!(w.liquid(-3, 0), Liquid::NONE);
    }

    #[test]
    fn chunk_roundtrip() {
        let mut w = World::new(200, 130); // 130 -> ragged bottom chunk row
        let mut rng = Pcg32::new(0xfeed);
        for y in 0..w.height {
            for x in 0..w.width {
                let id = TileId::ALL[rng.gen_range_u32(0..TileId::COUNT as u32) as usize];
                let liquid = match rng.gen_range_u32(0..4) {
                    0 => Liquid::new(LiquidKind::Water, rng.gen_range_u32(1..9) as u8),
                    1 => Liquid::new(LiquidKind::Lava, rng.gen_range_u32(1..9) as u8),
                    _ => Liquid::NONE,
                };
                let t = Tile {
                    id,
                    wall: WallId::ALL[rng.gen_range_u32(0..WallId::COUNT as u32) as usize],
                    liquid,
                    state: state::part(rng.gen_range_u32(0..8) as u8, 1),
                };
                w.set_tile(x, y, t);
            }
        }

        for cy in 0..w.chunks_y() {
            for cx in 0..w.chunks_x() {
                let encoded = w.encode_chunk(cx, cy);
                let decoded = decode_chunk(&encoded).expect("decode");
                assert_eq!(decoded.len(), (CHUNK_SIZE * CHUNK_SIZE) as usize);
                for (i, &t) in decoded.iter().enumerate() {
                    let x = cx * CHUNK_SIZE + i as u32 % CHUNK_SIZE;
                    let y = cy * CHUNK_SIZE + i as u32 / CHUNK_SIZE;
                    assert_eq!(t, w.tile(x, y), "mismatch at ({x},{y})");
                }
            }
        }

        // apply_chunk mirrors encode/decode.
        let encoded = w.encode_chunk(1, 1);
        let decoded = decode_chunk(&encoded).expect("decode");
        let mut mirror = World::new(200, 130);
        mirror.apply_chunk(1, 1, &decoded);
        for dy in 0..CHUNK_SIZE {
            for dx in 0..CHUNK_SIZE {
                let (x, y) = (CHUNK_SIZE + dx, CHUNK_SIZE + dy);
                assert_eq!(mirror.tile(x, y), w.tile(x, y));
            }
        }
    }

    #[test]
    fn chunk_decode_rejects_garbage() {
        assert_eq!(decode_chunk(&[1, 2, 3]), Err(ChunkDecodeError::Compression));
        let short = lz4_flex::compress_prepend_size(&[0u8; 16]);
        assert_eq!(decode_chunk(&short), Err(ChunkDecodeError::WrongSize));
        let mut raw = vec![0u8; (CHUNK_SIZE * CHUNK_SIZE * 4) as usize];
        raw[0] = 250; // invalid tile id
        let bad = lz4_flex::compress_prepend_size(&raw);
        assert_eq!(decode_chunk(&bad), Err(ChunkDecodeError::InvalidTile));
    }

    #[test]
    fn place_multitile_sets_part_frames() {
        let mut w = World::new(32, 32);
        // Bed is 4×2.
        assert!(w.place_multitile(10, 10, TileId::Bed));
        for dy in 0..2u32 {
            for dx in 0..4u32 {
                let t = w.tile(10 + dx, 10 + dy);
                assert_eq!(t.id, TileId::Bed);
                assert_eq!(state::part_x(t.state) as u32, dx);
                assert_eq!(state::part_y(t.state) as u32, dy);
                assert_eq!(w.multitile_origin(10 + dx, 10 + dy), (10, 10));
            }
        }
        // Overlap and out-of-bounds placements are rejected atomically.
        assert!(!w.place_multitile(12, 10, TileId::Chest));
        assert!(!w.place_multitile(31, 30, TileId::Chest)); // x+1 out of bounds
        assert_eq!(w.tile(31, 30), Tile::AIR);
        assert_eq!(w.tile(31, 31), Tile::AIR);
        // Walls and liquid survive placement.
        let mut cell = Tile::AIR;
        cell.wall = WallId::Stone;
        cell.liquid = Liquid::new(LiquidKind::Water, 4);
        w.set_tile(20, 20, cell);
        assert!(w.place_multitile(20, 20, TileId::Torch));
        assert_eq!(w.tile(20, 20).wall, WallId::Stone);
        assert_eq!(w.tile(20, 20).liquid, Liquid::new(LiquidKind::Water, 4));
    }

    #[test]
    fn place_multitile_keeps_wall_placed_bit() {
        // Regression: placing furniture over a player-placed wall used to
        // clear state::WALL_PLACED, reclassifying the wall as natural (so
        // hammering it would wrongly drop nothing, §2).
        let mut w = World::new(32, 32);
        let mut cell = Tile::AIR;
        cell.wall = WallId::Wood;
        cell.state = state::WALL_PLACED;
        w.set_tile(10, 10, cell);
        w.set_tile(11, 10, cell);
        assert!(w.place_multitile(10, 10, TileId::Workbench)); // 2×1
        for dx in 0..2u32 {
            let t = w.tile(10 + dx, 10);
            assert_eq!(t.id, TileId::Workbench);
            assert_ne!(t.state & state::WALL_PLACED, 0, "wall bit lost at dx={dx}");
            assert_eq!(state::part_x(t.state) as u32, dx);
            assert_eq!(state::part_y(t.state), 0);
        }
        // Cells without the bit don't gain it.
        assert!(w.place_multitile(20, 20, TileId::Workbench));
        assert_eq!(w.tile(20, 20).state & state::WALL_PLACED, 0);
    }

    #[test]
    fn multitile_origin_ignores_sprite_variants_on_1x1_tiles() {
        // Regression: state bits 0–2 double as sprite variants on 1×1 tiles
        // (grass mushroom, tree-top); multitile_origin must not read them as
        // part offsets.
        let mut w = World::new(16, 16);
        let mut grass = Tile::of(TileId::Grass);
        grass.state = state::GRASS_MUSHROOM;
        w.set_tile(5, 5, grass);
        assert_eq!(w.multitile_origin(5, 5), (5, 5));

        let mut top = Tile::of(TileId::TreeTrunk);
        top.state = state::TREE_SEGMENT_TOP;
        w.set_tile(7, 3, top);
        assert_eq!(w.multitile_origin(7, 3), (7, 3));

        // Real multi-tiles still resolve through their frame bytes.
        assert!(w.place_multitile(9, 9, TileId::Bed));
        assert_eq!(w.multitile_origin(12, 10), (9, 9));
    }

    #[test]
    fn day_night_boundaries() {
        assert!(!is_day(0)); // midnight
        assert!(!is_day(DAWN_TICK - 1));
        assert!(is_day(DAWN_TICK)); // 4:30 AM
        assert!(is_day(NEW_WORLD_TIME)); // 8:15 AM
        assert!(is_day(DUSK_TICK - 1));
        assert!(!is_day(DUSK_TICK)); // 7:30 PM
        assert!(World::new(10, 10).is_day()); // new worlds start in daytime
    }
}
