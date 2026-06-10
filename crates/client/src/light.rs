//! Client-side flood-fill lighting (DESIGN §10) and the day/night sky-light
//! ramp (§9).
//!
//! Pure logic — no macroquad in here, so the whole engine unit-tests
//! natively. Rendering reads the result through [`LightEngine::light_at_tile`]
//! / [`LightEngine::corner_light`] / [`LightEngine::brightness_at`].
//!
//! Model (all numbers from `shared::tiles` / DESIGN §10):
//! - Per-tile light 0–32. Sources: tile emissions from `TILE_DATA`, lava
//!   fluid cells at [`LAVA_LIGHT`], sky-exposed liquid-free non-solid cells
//!   at [`sky_light`]`(time)` (§10: sunlight seeds *air*, so deep water dims
//!   with depth), and dynamic per-frame sources (player glow, Mining Helmet)
//!   passed into [`LightEngine::update`].
//! - BFS propagation, 4-directional, −2 per step into non-solid, −6 into
//!   solid, max-combine where fields overlap.
//! - Recompute is chunk-gated: 32×32 *light* chunks (distinct from the 64×64
//!   network chunks) are dirtied by tile deltas (the changed chunk + its 8
//!   neighbors) and by sky-exposure changes along edited columns; only the
//!   visible-area ±2 chunks are ever recomputed, in one flood pass over that
//!   region plus a 16-tile margin so sources just outside still shine in.

use std::collections::HashSet;

use ferraria_shared::tiles::{
    LiquidKind, LAVA_LIGHT, LIGHT_ATTEN_AIR, LIGHT_ATTEN_SOLID, LIGHT_MAX, SKY_LIGHT_DAY,
    SKY_LIGHT_NIGHT, SKY_LIGHT_RAMP_TICKS,
};
use ferraria_shared::world::{World, CHUNK_SIZE, DAWN_TICK, DUSK_TICK};

/// Light chunks are 32×32 tiles (§10).
pub const LIGHT_CHUNK: u32 = 32;

/// Recompute covers the visible area plus this many light chunks on every
/// side (§10: "visible-area ±2 chunks").
const REGION_PAD_CHUNKS: u32 = 2;

/// Sources outside the recomputed region still shine into it: the brightest
/// source (32) travelling through air fades by 2/tile, so 16 tiles of margin
/// captures every external contribution exactly.
const SOURCE_MARGIN: u32 = (LIGHT_MAX / LIGHT_ATTEN_AIR) as u32;

/// While a dawn/dusk ramp is moving, sky-exposed areas re-light at least
/// this often, in ticks (§10).
const SKY_REFRESH_TICKS: u64 = 10;

// ---- Day/night sky light (§9/§10) ------------------------------------------

/// 0 at full night, 1 at full day, ramping linearly across the 30 in-game
/// minutes centered on dawn/dusk.
pub fn daylight(time: u32) -> f32 {
    let t = time as f32;
    let ramp = SKY_LIGHT_RAMP_TICKS as f32;
    let half = ramp / 2.0;
    let rise = ((t - (DAWN_TICK as f32 - half)) / ramp).clamp(0.0, 1.0);
    let fall = ((t - (DUSK_TICK as f32 - half)) / ramp).clamp(0.0, 1.0);
    (rise - fall).clamp(0.0, 1.0)
}

/// Sky-light level for a tick-of-day: 32 at full day, 8 at full night,
/// linear across the dawn/dusk ramps (§10).
pub fn sky_light(time: u32) -> u8 {
    let night = SKY_LIGHT_NIGHT as f32;
    let day = SKY_LIGHT_DAY as f32;
    (night + (day - night) * daylight(time)).round() as u8
}

/// Whether the sky light is currently ramping (inside a dawn/dusk window).
fn in_ramp(time: u32) -> bool {
    let half = SKY_LIGHT_RAMP_TICKS / 2;
    let near = |edge: u32| time >= edge.saturating_sub(half) && time <= edge + half;
    near(DAWN_TICK) || near(DUSK_TICK)
}

// ---- Engine -----------------------------------------------------------------

/// A non-tile light source for the current frame (player ambient glow 4,
/// Mining Helmet 20 at the wearer's head — §10), in tile coords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DynamicSource {
    pub x: i32,
    pub y: i32,
    pub level: u8,
}

pub struct LightEngine {
    width: u32,
    height: u32,
    /// Per-tile light 0–32, row-major like `World::tiles`. Only cells inside
    /// the last recomputed region are current — everything rendered lies
    /// inside it.
    light: Vec<u8>,
    /// Per-column row of the topmost solid tile (`height` = fully open).
    /// `y < top_solid[x]` ⇔ sky-exposed (§10 sunlight rule).
    top_solid: Vec<u32>,
    /// Dirty light chunks awaiting a recompute that covers them.
    dirty: HashSet<(u32, u32)>,
    /// Bright-to-dark bucket queue, kept around so recomputes don't allocate.
    buckets: Vec<Vec<u32>>,
    /// State of the last recompute, to detect when another one is needed.
    /// `last_sources` holds only the sources that could affect the region
    /// (see the filter in [`LightEngine::update`]).
    last_sources: Vec<DynamicSource>,
    last_sky: u8,
    last_region: (u32, u32, u32, u32),
    last_sky_refresh: u64,
    computed_once: bool,
    /// Scratch for the per-frame source filter (allocation reuse).
    cur_sources: Vec<DynamicSource>,
}

impl LightEngine {
    pub fn new(width: u32, height: u32) -> LightEngine {
        LightEngine {
            width,
            height,
            light: vec![0; width as usize * height as usize],
            // Nothing loaded reads as air, i.e. open sky, matching the world
            // mirror's semantics; real chunks re-derive columns on arrival.
            top_solid: vec![height; width as usize],
            dirty: HashSet::new(),
            buckets: vec![Vec::new(); LIGHT_MAX as usize + 1],
            last_sources: Vec::new(),
            last_sky: 0,
            last_region: (u32::MAX, u32::MAX, u32::MAX, u32::MAX),
            last_sky_refresh: 0,
            computed_once: false,
            cur_sources: Vec::new(),
        }
    }

    #[inline]
    fn idx(&self, x: u32, y: u32) -> usize {
        y as usize * self.width as usize + x as usize
    }

    /// Light level at a tile, clamped into the world.
    pub fn light_at_tile(&self, x: i32, y: i32) -> u8 {
        let x = x.clamp(0, self.width as i32 - 1) as u32;
        let y = y.clamp(0, self.height as i32 - 1) as u32;
        self.light[self.idx(x, y)]
    }

    /// 0–1 brightness at a tile-space point. **Merge hook**: tint entities,
    /// item drops, and remote players by multiplying their sprite colors by
    /// this, sampled at their center.
    pub fn brightness_at(&self, x: f32, y: f32) -> f32 {
        self.light_at_tile(x.floor() as i32, y.floor() as i32) as f32 / LIGHT_MAX as f32
    }

    /// 0–1 brightness at the tile-grid corner (x, y): mean of the 4 tiles
    /// meeting there. Drives the smooth per-corner shading.
    pub fn corner_light(&self, x: i64, y: i64) -> f32 {
        let mut sum = 0u32;
        for (dx, dy) in [(-1, -1), (0, -1), (-1, 0), (0, 0)] {
            sum += self.light_at_tile((x + dx) as i32, (y + dy) as i32) as u32;
        }
        sum as f32 / (4.0 * LIGHT_MAX as f32)
    }

    /// Sky-exposed: no solid tile anywhere above (§10).
    pub fn sky_exposed(&self, x: u32, y: u32) -> bool {
        x < self.width && y < self.top_solid[x as usize]
    }

    fn chunk_grid(&self) -> (u32, u32) {
        (
            self.width.div_ceil(LIGHT_CHUNK),
            self.height.div_ceil(LIGHT_CHUNK),
        )
    }

    /// Dirties light chunk (cx, cy) and its 8 neighbors (§10).
    fn mark_chunk_3x3(&mut self, cx: u32, cy: u32) {
        let (cw, ch) = self.chunk_grid();
        for ny in cy.saturating_sub(1)..=(cy + 1).min(ch - 1) {
            for nx in cx.saturating_sub(1)..=(cx + 1).min(cw - 1) {
                self.dirty.insert((nx, ny));
            }
        }
    }

    /// Dirties the 3×3 light-chunk neighborhood around tile (x, y).
    pub fn mark_dirty_around(&mut self, x: u32, y: u32) {
        self.mark_chunk_3x3(x / LIGHT_CHUNK, y / LIGHT_CHUNK);
    }

    /// First solid row of column x (world mirror semantics: unloaded = air).
    fn scan_column(&self, world: &World, x: u32) -> u32 {
        (0..self.height)
            .find(|&y| world.tile(x, y).is_solid())
            .unwrap_or(self.height)
    }

    /// Re-derives one column's sky exposure after an edit, dirtying every
    /// chunk along the exposure change (uncovering a shaft must relight far
    /// below the edited tile, not just its 3×3 neighborhood).
    fn refresh_column(&mut self, world: &World, x: u32) {
        let old = self.top_solid[x as usize];
        let new = self.scan_column(world, x);
        if old == new {
            return;
        }
        self.top_solid[x as usize] = new;
        let lo = old.min(new) / LIGHT_CHUNK;
        let hi = old.max(new).min(self.height - 1) / LIGHT_CHUNK;
        for cy in lo..=hi {
            self.mark_chunk_3x3(x / LIGHT_CHUNK, cy);
        }
    }

    /// Call after any tile delta (`TileChanged` and friends) lands in the
    /// world mirror.
    pub fn on_tile_changed(&mut self, world: &World, x: u32, y: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        self.refresh_column(world, x);
        self.mark_dirty_around(x, y);
    }

    /// Call after a 64×64 network chunk is applied to the world mirror:
    /// re-derives its columns' sky exposure and dirties its light chunks plus
    /// the neighbor ring.
    pub fn on_chunk_applied(&mut self, world: &World, cx: u32, cy: u32) {
        let x0 = (cx * CHUNK_SIZE).min(self.width);
        let x1 = ((cx + 1) * CHUNK_SIZE).min(self.width);
        for x in x0..x1 {
            self.refresh_column(world, x);
        }
        let (cw, ch) = self.chunk_grid();
        let lx0 = (cx * CHUNK_SIZE / LIGHT_CHUNK).saturating_sub(1);
        let ly0 = (cy * CHUNK_SIZE / LIGHT_CHUNK).saturating_sub(1);
        let lx1 = (((cx + 1) * CHUNK_SIZE - 1) / LIGHT_CHUNK + 1).min(cw - 1);
        let ly1 = (((cy + 1) * CHUNK_SIZE - 1) / LIGHT_CHUNK + 1).min(ch - 1);
        for ly in ly0..=ly1 {
            for lx in lx0..=lx1 {
                self.dirty.insert((lx, ly));
            }
        }
    }

    /// Any sky-exposed cell inside the chunk-rect region? (If a ramp is
    /// moving but the whole region is buried, there is nothing to refresh.)
    fn region_has_sky(&self, cx0: u32, cx1: u32, cy0: u32) -> bool {
        let x0 = cx0 * LIGHT_CHUNK;
        let x1 = ((cx1 + 1) * LIGHT_CHUNK).min(self.width);
        let y0 = cy0 * LIGHT_CHUNK;
        (x0..x1).any(|x| self.top_solid[x as usize] > y0)
    }

    /// Per-frame entry point: decides whether the visible region needs a
    /// recompute (dirty chunks, sky-light ramp, moved dynamic sources, or
    /// camera entering new chunks) and runs it. Returns whether a recompute
    /// happened — the caller times it for the F3 overlay.
    ///
    /// Dynamic sources are filtered to the margin-expanded recompute region
    /// before the moved-since-last-frame comparison: [`Self::recompute`]
    /// ignores everything outside it, so a remote player walking around far
    /// off-screen must not force full recomputes whose output is identical.
    ///
    /// `view` is the visible tile rect (x0, y0, x1, y1; inclusive),
    /// `abs_tick` a monotonically increasing tick counter.
    pub fn update(
        &mut self,
        world: &World,
        view: (u32, u32, u32, u32),
        time: u32,
        abs_tick: u64,
        sources: &[DynamicSource],
    ) -> bool {
        let (cw, ch) = self.chunk_grid();
        let cx0 = (view.0 / LIGHT_CHUNK).saturating_sub(REGION_PAD_CHUNKS);
        let cy0 = (view.1 / LIGHT_CHUNK).saturating_sub(REGION_PAD_CHUNKS);
        let cx1 = (view.2 / LIGHT_CHUNK + REGION_PAD_CHUNKS).min(cw - 1);
        let cy1 = (view.3 / LIGHT_CHUNK + REGION_PAD_CHUNKS).min(ch - 1);
        let in_region = |cx: u32, cy: u32| cx0 <= cx && cx <= cx1 && cy0 <= cy && cy <= cy1;
        let rect = (
            cx0 * LIGHT_CHUNK,
            cy0 * LIGHT_CHUNK,
            ((cx1 + 1) * LIGHT_CHUNK - 1).min(self.width - 1),
            ((cy1 + 1) * LIGHT_CHUNK - 1).min(self.height - 1),
        );

        // Keep only the sources that can shine into the region (doc above).
        let (fx0, fy0, fx1, fy1) = self.expanded(rect);
        let mut cur = std::mem::take(&mut self.cur_sources);
        cur.clear();
        cur.extend(sources.iter().copied().filter(|s| {
            fx0 as i32 <= s.x && s.x <= fx1 as i32 && fy0 as i32 <= s.y && s.y <= fy1 as i32
        }));

        let sky = sky_light(time);
        let mut go = !self.computed_once
            || sky != self.last_sky
            || (cx0, cy0, cx1, cy1) != self.last_region
            || cur != self.last_sources
            || self.dirty.iter().any(|&(cx, cy)| in_region(cx, cy));
        // §10: sky-exposed chunks re-dirty every 10 ticks while a ramp is
        // moving (the integer sky level itself only steps every ~75 ticks,
        // which `sky != last_sky` already catches).
        if !go
            && in_ramp(time)
            && abs_tick.saturating_sub(self.last_sky_refresh) >= SKY_REFRESH_TICKS
            && self.region_has_sky(cx0, cx1, cy0)
        {
            go = true;
        }
        if !go {
            self.cur_sources = cur;
            return false;
        }

        self.recompute(world, rect, sky, &cur);
        self.dirty.retain(|&(cx, cy)| !in_region(cx, cy));
        std::mem::swap(&mut self.last_sources, &mut cur);
        self.cur_sources = cur;
        self.last_sky = sky;
        self.last_region = (cx0, cy0, cx1, cy1);
        self.last_sky_refresh = abs_tick;
        self.computed_once = true;
        true
    }

    /// `rect` (inclusive tile bounds) expanded by [`SOURCE_MARGIN`] on every
    /// side, clamped to the world: exactly the area [`Self::recompute`]
    /// floods, and hence the only area whose sources can affect `rect`.
    fn expanded(&self, rect: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        (
            rect.0.saturating_sub(SOURCE_MARGIN),
            rect.1.saturating_sub(SOURCE_MARGIN),
            (rect.2 + SOURCE_MARGIN).min(self.width - 1),
            (rect.3 + SOURCE_MARGIN).min(self.height - 1),
        )
    }

    /// One full flood pass over `rect` (inclusive tile bounds) expanded by
    /// [`SOURCE_MARGIN`]: seed all sources, then BFS bright-to-dark through
    /// a 33-bucket queue — each cell settles at its final (max-combined)
    /// level the first time it pops, because pushes only ever go to strictly
    /// lower buckets.
    fn recompute(
        &mut self,
        world: &World,
        rect: (u32, u32, u32, u32),
        sky: u8,
        sources: &[DynamicSource],
    ) {
        debug_assert_eq!((world.width, world.height), (self.width, self.height));
        let (fx0, fy0, fx1, fy1) = self.expanded(rect);
        let (ex0, ey0, ex1, ey1) = (fx0 as usize, fy0 as usize, fx1 as usize, fy1 as usize);
        let w = self.width as usize;

        // Seed: tile emissions, lava, sky columns.
        for y in ey0..=ey1 {
            let row = y * w;
            for x in ex0..=ex1 {
                let t = world.tiles[row + x];
                let mut s = t.id.data().light;
                if t.liquid.kind() == Some(LiquidKind::Lava) {
                    s = s.max(LAVA_LIGHT);
                }
                // §10: sunlight sources are *air* tiles open to the sky.
                // Liquid cells are excluded — they get propagated light
                // (−2/step) instead, so deep water dims with depth.
                if s < sky
                    && !t.is_solid()
                    && t.liquid.kind().is_none()
                    && (y as u32) < self.top_solid[x]
                {
                    s = sky;
                }
                self.light[row + x] = s;
                if s > 0 {
                    self.buckets[s as usize].push((row + x) as u32);
                }
            }
        }
        for src in sources {
            if src.x < ex0 as i32 || src.x > ex1 as i32 || src.y < ey0 as i32 || src.y > ey1 as i32
            {
                continue;
            }
            let i = self.idx(src.x as u32, src.y as u32);
            if src.level > self.light[i] {
                self.light[i] = src.level;
                self.buckets[src.level as usize].push(i as u32);
            }
        }

        // Flood, brightest first.
        for level in (1..=LIGHT_MAX).rev() {
            let mut queue = std::mem::take(&mut self.buckets[level as usize]);
            for &iu in &queue {
                let i = iu as usize;
                if self.light[i] != level {
                    continue; // raised after being queued; stale entry
                }
                let (x, y) = (i % w, i / w);
                let mut spread = |ni: usize| {
                    let atten = if world.tiles[ni].is_solid() {
                        LIGHT_ATTEN_SOLID
                    } else {
                        LIGHT_ATTEN_AIR
                    };
                    let nl = level.saturating_sub(atten);
                    if nl > self.light[ni] {
                        self.light[ni] = nl;
                        self.buckets[nl as usize].push(ni as u32);
                    }
                };
                if x > ex0 {
                    spread(i - 1);
                }
                if x < ex1 {
                    spread(i + 1);
                }
                if y > ey0 {
                    spread(i - w);
                }
                if y < ey1 {
                    spread(i + w);
                }
            }
            // Hand the buffer back so future recomputes reuse its capacity.
            queue.clear();
            self.buckets[level as usize] = queue;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferraria_shared::tiles::{
        Liquid, Tile, TileId, LIQUID_MAX_LEVEL, MINING_HELMET_LIGHT, PLAYER_GLOW,
    };
    use ferraria_shared::world::{DAY_TICKS, NEW_WORLD_TIME};

    /// A world with a solid stone lid on row 0 so no column is sky-exposed
    /// (isolates point sources from sunlight).
    fn capped_world(w: u32, h: u32) -> World {
        let mut world = World::new(w, h);
        for x in 0..w {
            world.set_tile(x, 0, Tile::of(TileId::Stone));
        }
        world
    }

    /// An engine with columns derived from the world, lighting recomputed
    /// over the whole world with the given sky level and extra sources.
    fn lit(world: &World, sky: u8, sources: &[DynamicSource]) -> LightEngine {
        let mut e = LightEngine::new(world.width, world.height);
        for x in 0..world.width {
            e.top_solid[x as usize] = e.scan_column(world, x);
        }
        e.recompute(
            world,
            (0, 0, world.width - 1, world.height - 1),
            sky,
            sources,
        );
        e
    }

    #[test]
    fn torch_in_open_air_lights_radius_14() {
        let mut world = capped_world(80, 80);
        world.set_tile(40, 40, Tile::of(TileId::Torch));
        let e = lit(&world, 0, &[]);
        assert_eq!(e.light_at_tile(40, 40), 28); // Torch emits 28 (§10)
                                                 // −2 per air step, 4-dir BFS ⇒ level 28 − 2·d at Manhattan distance d.
        assert_eq!(e.light_at_tile(53, 40), 2); // d = 13: dimmest lit ring
        assert_eq!(e.light_at_tile(40, 27), 2);
        assert_eq!(e.light_at_tile(47, 34), 2); // diagonal, d = 7 + 6 = 13
        assert_eq!(e.light_at_tile(54, 40), 0); // d = 14: dark
        assert_eq!(e.light_at_tile(47, 47), 0); // d = 14 diagonally: dark
                                                // Lit span along the row: 13 left + torch + 13 right = 27 cells, the
                                                // §10 "radius 14" counting the torch tile itself.
        let lit_cells = (0..80).filter(|&x| e.light_at_tile(x, 40) > 0).count();
        assert_eq!(lit_cells, 27);
    }

    #[test]
    fn light_through_five_solid_tiles_dies_out() {
        // Solid rock everywhere (so nothing sneaks around through air) with
        // a torch embedded in it.
        let mut world = World::new(64, 64);
        for y in 0..world.height {
            for x in 0..world.width {
                world.set_tile(x, y, Tile::of(TileId::Stone));
            }
        }
        world.set_tile(10, 10, Tile::of(TileId::Torch));
        let e = lit(&world, 0, &[]);
        // −6 entering each solid: 28 → 22, 16, 10, 4, 0.
        assert_eq!(e.light_at_tile(11, 10), 22);
        assert_eq!(e.light_at_tile(14, 10), 4);
        assert_eq!(e.light_at_tile(15, 10), 0); // dead inside the 5th solid
        assert_eq!(e.light_at_tile(16, 10), 0);
        assert_eq!(e.light_at_tile(10, 15), 0); // same straight down
    }

    #[test]
    fn overlapping_sources_max_combine() {
        let mut world = capped_world(64, 64);
        world.set_tile(20, 30, Tile::of(TileId::Torch));
        world.set_tile(30, 30, Tile::of(TileId::Torch));
        let e = lit(&world, 0, &[]);
        // Midpoint: 5 steps from both ⇒ max(18, 18); fields don't add.
        assert_eq!(e.light_at_tile(25, 30), 18);
        // Off-center: the nearer torch wins — max(24, 12) = 24.
        assert_eq!(e.light_at_tile(22, 30), 24);
        // A lava cell under one torch: max(torch-2 entering it... , 18).
        let mut t = world.tile(20, 31);
        t.liquid = Liquid::new(LiquidKind::Lava, 8);
        world.set_tile(20, 31, t);
        let e = lit(&world, 0, &[]);
        assert_eq!(e.light_at_tile(20, 31), 26.max(LAVA_LIGHT)); // 28−2 beats 18
        assert_eq!(e.light_at_tile(20, 33), 22); // ...and keeps fading by −2
    }

    #[test]
    fn dynamic_player_sources_light_their_tiles() {
        // Far enough apart (Manhattan 32) that the helmet's field (radius
        // ≤ 10) can't reach the glow's neighborhood.
        let world = capped_world(40, 40);
        let glow = DynamicSource {
            x: 8,
            y: 8,
            level: PLAYER_GLOW,
        };
        let helmet = DynamicSource {
            x: 24,
            y: 24,
            level: MINING_HELMET_LIGHT,
        };
        let e = lit(&world, 0, &[glow, helmet]);
        assert_eq!(e.light_at_tile(8, 8), PLAYER_GLOW);
        assert_eq!(e.light_at_tile(9, 8), PLAYER_GLOW - 2);
        assert_eq!(e.light_at_tile(10, 8), 0); // glow 4 only carries 1 tile
        assert_eq!(e.light_at_tile(24, 24), MINING_HELMET_LIGHT);
        assert_eq!(e.light_at_tile(29, 24), MINING_HELMET_LIGHT - 10);
    }

    #[test]
    fn blocked_sky_column_contributes_nothing_below() {
        // One column wide so nothing leaks in from the sides: a single solid
        // tile at y = 5 must stop the column being a sky source below it.
        let mut world = World::new(1, 50);
        world.set_tile(0, 5, Tile::of(TileId::Stone));
        let e = lit(&world, SKY_LIGHT_DAY, &[]);
        for y in 0..5 {
            assert_eq!(e.light_at_tile(0, y), 32, "open sky at y={y}");
        }
        assert!(!e.sky_exposed(0, 5));
        // Below the lid: never 32 again — only propagated light, fading out.
        assert_eq!(e.light_at_tile(0, 5), 26); // 32 − 6 entering the solid
        assert_eq!(e.light_at_tile(0, 6), 24);
        assert_eq!(e.light_at_tile(0, 17), 2);
        assert_eq!(e.light_at_tile(0, 18), 0);
        assert_eq!(e.light_at_tile(0, 40), 0); // pitch black at depth
    }

    #[test]
    fn dusk_ramp_boundary_values() {
        let half = SKY_LIGHT_RAMP_TICKS / 2;
        // Dusk: 32 entering the ramp, 20 at the midpoint, 8 leaving it.
        assert_eq!(sky_light(DUSK_TICK - half), SKY_LIGHT_DAY);
        assert_eq!(sky_light(DUSK_TICK), 20);
        assert_eq!(sky_light(DUSK_TICK + half), SKY_LIGHT_NIGHT);
        // Dawn mirrors it.
        assert_eq!(sky_light(DAWN_TICK - half), SKY_LIGHT_NIGHT);
        assert_eq!(sky_light(DAWN_TICK), 20);
        assert_eq!(sky_light(DAWN_TICK + half), SKY_LIGHT_DAY);
        // Far from the ramps.
        assert_eq!(sky_light(NEW_WORLD_TIME), SKY_LIGHT_DAY);
        assert_eq!(sky_light(0), SKY_LIGHT_NIGHT); // midnight
        assert_eq!(sky_light(DAY_TICKS - 1), SKY_LIGHT_NIGHT);
        assert!((daylight(DUSK_TICK) - 0.5).abs() < 1e-6);
        assert!(in_ramp(DUSK_TICK) && in_ramp(DAWN_TICK));
        assert!(!in_ramp(NEW_WORLD_TIME) && !in_ramp(0));
    }

    #[test]
    fn dirty_propagation_marks_exactly_the_3x3_neighborhood() {
        // Solid lid on row 0 keeps column heights stable, so the tile edit
        // dirties exactly the changed chunk + 8 neighbors and nothing else.
        let mut world = capped_world(256, 256);
        let mut e = LightEngine::new(world.width, world.height);
        for x in 0..world.width {
            e.top_solid[x as usize] = e.scan_column(&world, x);
        }
        world.set_tile(70, 70, Tile::of(TileId::Stone));
        e.on_tile_changed(&world, 70, 70); // tile (70,70) → light chunk (2,2)
        let mut expect = HashSet::new();
        for cy in 1..=3 {
            for cx in 1..=3 {
                expect.insert((cx, cy));
            }
        }
        assert_eq!(e.dirty, expect);

        // Corner chunk: the 3×3 clamps to the world edge (2×2).
        let mut e2 = LightEngine::new(world.width, world.height);
        for x in 0..world.width {
            e2.top_solid[x as usize] = e2.scan_column(&world, x);
        }
        world.set_tile(1, 1, Tile::of(TileId::Stone));
        e2.on_tile_changed(&world, 1, 1);
        // Row 0 is already solid, so column exposure is unchanged and only
        // the clamped 2×2 corner neighborhood is marked.
        let expect2: HashSet<_> = [(0, 0), (1, 0), (0, 1), (1, 1)].into_iter().collect();
        assert_eq!(e2.dirty, expect2);
    }

    #[test]
    fn uncovering_a_shaft_dirties_the_whole_exposure_span() {
        // A lone solid tile at (8, 4) shades the column; removing it must
        // dirty chunks all the way down the newly exposed span, not just the
        // 3×3 around the edit.
        let mut world = World::new(64, 256);
        world.set_tile(8, 4, Tile::of(TileId::Stone));
        let mut e = LightEngine::new(world.width, world.height);
        for x in 0..world.width {
            e.top_solid[x as usize] = e.scan_column(&world, x);
        }
        assert_eq!(e.top_solid[8], 4);
        world.set_tile(8, 4, Tile::AIR);
        e.on_tile_changed(&world, 8, 4);
        assert_eq!(e.top_solid[8], world.height); // fully open again
        assert!(e.dirty.contains(&(0, 7)), "deep chunk along the shaft");
        assert!(e.dirty.contains(&(1, 7)), "neighbor of the deep chunk");
    }

    #[test]
    fn update_recomputes_visible_region_and_clears_dirty() {
        let mut world = capped_world(256, 256);
        world.set_tile(100, 100, Tile::of(TileId::Torch));
        let mut e = LightEngine::new(world.width, world.height);
        e.on_chunk_applied(&world, 1, 1); // pretend the chunk streamed in
        let view = (80, 80, 120, 110);
        assert!(e.update(&world, view, 0, 0, &[]));
        assert_eq!(e.light_at_tile(100, 100), 28);
        assert_eq!(e.light_at_tile(100, 105), 18);
        // Nothing changed: the next update is a no-op...
        assert!(!e.update(&world, view, 0, 1, &[]));
        // ...but a moved dynamic source forces one.
        let src = [DynamicSource {
            x: 90,
            y: 100,
            level: PLAYER_GLOW,
        }];
        assert!(e.update(&world, view, 0, 2, &src));
        assert!(!e.update(&world, view, 0, 3, &src));
    }

    #[test]
    fn far_offscreen_source_movement_does_not_force_recompute() {
        // View (0,0,80,45) ⇒ region chunks (0,0)..(4,3) ⇒ rect x ≤ 159;
        // + SOURCE_MARGIN (16) ⇒ only sources with x ≤ 175 can matter.
        let world = World::new(512, 128);
        let mut e = LightEngine::new(world.width, world.height);
        let view = (0, 0, 80, 45);
        let t = NEW_WORLD_TIME; // full day, outside the ramps
        let near = DynamicSource {
            x: 40,
            y: 20,
            level: PLAYER_GLOW,
        };
        let far = |x: i32| DynamicSource {
            x,
            y: 20,
            level: PLAYER_GLOW,
        };
        assert!(e.update(&world, view, t, 0, &[near, far(400)]));
        // A remote player walking far off-screen: recompute output would be
        // identical, so none may run (one full recompute per tile crossing
        // defeated the gating before the filter).
        assert!(!e.update(&world, view, t, 1, &[near, far(401)]));
        assert!(!e.update(&world, view, t, 2, &[near, far(450)]));
        // It leaving the world entirely changes nothing either.
        assert!(!e.update(&world, view, t, 3, &[near]));
        // A source inside the margin-expanded region still gates correctly:
        // appearing/moving there forces exactly one recompute.
        assert!(e.update(&world, view, t, 4, &[near, far(170)]));
        assert!(!e.update(&world, view, t, 5, &[near, far(170)]));
        assert!(e.update(&world, view, t, 6, &[near, far(160)]));
        // And the near (on-screen) source moving recomputes as before.
        let near2 = DynamicSource { x: 41, ..near };
        assert!(e.update(&world, view, t, 7, &[near2, far(160)]));
    }

    #[test]
    fn deep_water_dims_with_depth_instead_of_full_sky_light() {
        // One open column with water from y = 10 down: liquid cells are not
        // sunlight sources (§10 seeds *air* tiles), so light propagates in
        // from the surface at −2/step instead of staying 32 to the bottom.
        let mut world = World::new(1, 40);
        for y in 10..40 {
            let mut t = world.tile(0, y);
            t.liquid = Liquid::new(LiquidKind::Water, LIQUID_MAX_LEVEL);
            world.set_tile(0, y, t);
        }
        let e = lit(&world, SKY_LIGHT_DAY, &[]);
        assert_eq!(e.light_at_tile(0, 9), 32); // air above the surface
        assert_eq!(e.light_at_tile(0, 10), 30); // first water cell: 32 − 2
        assert_eq!(e.light_at_tile(0, 15), 20); // …fading by 2 per cell
        assert_eq!(e.light_at_tile(0, 24), 2); // dimmest lit depth
        assert_eq!(e.light_at_tile(0, 25), 0); // 16 cells down: dark
        assert_eq!(e.light_at_tile(0, 39), 0); // lake bottom: dark
    }

    #[test]
    fn ramp_rerefreshes_sky_exposed_region_every_10_ticks() {
        let world = World::new(128, 128); // fully open sky
        let mut e = LightEngine::new(world.width, world.height);
        let view = (0, 0, 100, 60);
        let t = DUSK_TICK; // mid-ramp; sky_light = 20 here
        assert!(e.update(&world, view, t, 1000, &[]));
        assert_eq!(e.light_at_tile(50, 50), 20);
        // Same tick value, < 10 ticks later: nothing to do.
        assert!(!e.update(&world, view, t, 1005, &[]));
        // ≥ 10 ticks into a ramp: re-dirties sky-exposed chunks (§10).
        assert!(e.update(&world, view, t, 1010, &[]));
        // Outside the ramp the 10-tick refresh is off.
        let day = NEW_WORLD_TIME;
        assert!(e.update(&world, view, day, 2000, &[])); // sky 32 ≠ 20
        assert!(!e.update(&world, view, day, 2020, &[]));
    }

    /// Rough native timing for a full visible-region recompute (the wasm
    /// budget is < 2 ms; the in-game number lives in the F3 overlay). Run
    /// with:
    /// `cargo test -p ferraria-client --release -- --ignored --nocapture`
    #[test]
    #[ignore = "perf measurement, run explicitly"]
    fn perf_full_visible_recompute() {
        // Terrain-ish 512×512 world: surface at y=200, caves and torches
        // sprinkled below deterministically.
        let mut world = World::new(512, 512);
        for y in 200..world.height {
            for x in 0..world.width {
                if (x * 31 + y * 17) % 11 != 0 {
                    world.set_tile(x, y, Tile::of(TileId::Stone));
                }
            }
        }
        for i in 0..60u32 {
            let h = star(i);
            world.set_tile(h % 512, 200 + (h >> 9) % 300, Tile::of(TileId::Torch));
        }
        fn star(i: u32) -> u32 {
            let mut h = i.wrapping_mul(0x9E37_79B9);
            h ^= h >> 15;
            h
        }
        let mut e = LightEngine::new(world.width, world.height);
        for x in 0..world.width {
            e.top_solid[x as usize] = e.scan_column(&world, x);
        }
        // A 1280×720 screen is 80×45 tiles; ±2 light chunks around it.
        let view = (216, 180, 296, 225);
        let sources = [DynamicSource {
            x: 256,
            y: 198,
            level: PLAYER_GLOW,
        }];
        let iters = 200;
        let t0 = std::time::Instant::now();
        for i in 0..iters {
            // Alternate sky so every iteration really recomputes.
            let time = if i % 2 == 0 {
                DUSK_TICK
            } else {
                DUSK_TICK + 75
            };
            assert!(e.update(&world, view, time, i as u64 * 100, &sources));
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
        println!("full visible-region recompute: {per:.3} ms (native)");
    }
}
