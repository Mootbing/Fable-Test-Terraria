//! The DESIGN §3 fluid cellular automaton (water + lava), shared by world
//! generation (settling pockets to equilibrium, pass 7) and the live
//! simulation (which calls [`FluidSim::step`] on the §3 cadence: water every
//! `tiles::WATER_UPDATE_TICKS`, lava every `tiles::LAVA_UPDATE_TICKS`).
//!
//! Rules per update, per liquid cell (§3):
//! 1. Flow down into the non-solid cell below until it is full.
//! 2. Else equalize with horizontal neighbors: move 1 level toward a lower
//!    neighbor. We canonize "lower" as *at least 2 levels lower* — a 1-level
//!    threshold makes adjacent 1/0 cells swap forever and never reach the
//!    equilibrium world generation needs; with the ≥2 threshold every flow
//!    strictly reduces a bounded potential, so settling terminates.
//! 3. Obsidian rule (§3.2): whenever a water cell and a lava cell become
//!    adjacent (or one flows into the other), the **lava** cell converts to
//!    an Obsidian tile and the water cell loses 1 level.
//!
//! Level-1 puddle evaporation (60 s, `tiles::PUDDLE_EVAPORATE_SECS`) is a
//! wall-clock rule and lives with the live sim's timers, not in the
//! automaton itself.
//!
//! The sim is sparse: only *active* cells are visited, and a cell stays
//! active only while its last update changed something. Worldgen fills
//! pockets to flat levels, so settling converges in a handful of rounds even
//! on a full-size world.

use ferraria_shared::tiles::{Liquid, LiquidKind, TileId, LIQUID_MAX_LEVEL};
use ferraria_shared::world::World;

/// Safety cap for [`settle`]: generation aborts settling after this many
/// rounds rather than spinning (never hit in practice; flat-filled pockets
/// settle in a few rounds).
const MAX_SETTLE_ROUNDS: u32 = 100_000;

/// Sparse fluid simulator over a [`World`]'s liquid layer.
pub struct FluidSim {
    width: u32,
    height: u32,
    /// Cells queued for the next update round.
    active: Vec<(u32, u32)>,
    /// Dedup flags, one per cell, indexed `y * width + x`.
    queued: Vec<bool>,
}

impl FluidSim {
    pub fn new(world: &World) -> FluidSim {
        FluidSim {
            width: world.width,
            height: world.height,
            active: Vec::new(),
            queued: vec![false; world.width as usize * world.height as usize],
        }
    }

    /// Marks a cell for re-evaluation (tile broken/placed, liquid added, ...).
    pub fn mark(&mut self, x: u32, y: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y * self.width + x) as usize;
        if !self.queued[i] {
            self.queued[i] = true;
            self.active.push((x, y));
        }
    }

    /// Marks every liquid cell in the world active (worldgen settling).
    pub fn seed_all(&mut self, world: &World) {
        for y in 0..world.height {
            for x in 0..world.width {
                if world.tile(x, y).liquid.is_some() {
                    self.mark(x, y);
                }
            }
        }
    }

    /// `true` while any cell still wants an update.
    pub fn is_active(&self) -> bool {
        !self.active.is_empty()
    }

    /// Runs one update round over the active set. `update_water` /
    /// `update_lava` implement the §3 cadence split (the live sim passes
    /// `tick % WATER_UPDATE_TICKS == 0` etc.; settling passes both `true`).
    /// Changed cells are appended to `changed` (for `TileChanged`
    /// broadcasts). Returns how many cells changed.
    pub fn step(
        &mut self,
        world: &mut World,
        update_water: bool,
        update_lava: bool,
        changed: &mut Vec<(u32, u32)>,
    ) -> u32 {
        let mut cells = std::mem::take(&mut self.active);
        // Deterministic order; bottom-up so columns drain in one round.
        cells.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for &(x, y) in &cells {
            self.queued[(y * self.width + x) as usize] = false;
        }

        let before = changed.len();
        for (x, y) in cells {
            let liquid = world.tile(x, y).liquid;
            let Some(kind) = liquid.kind() else {
                continue;
            };
            let due = match kind {
                LiquidKind::Water => update_water,
                LiquidKind::Lava => update_lava,
            };
            if !due {
                self.mark(x, y); // keep it queued for its next due round
                continue;
            }
            if self.update_cell(world, x, y, kind, liquid.level(), changed) {
                // Whatever moved may unbalance the neighborhood.
                self.mark(x, y);
                self.mark(x.wrapping_sub(1), y);
                self.mark(x + 1, y);
                self.mark(x, y.wrapping_sub(1));
                self.mark(x, y + 1);
            }
        }
        (changed.len() - before) as u32
    }

    /// One cell's update; returns `true` if anything changed.
    fn update_cell(
        &mut self,
        world: &mut World,
        x: u32,
        y: u32,
        kind: LiquidKind,
        level: u8,
        changed: &mut Vec<(u32, u32)>,
    ) -> bool {
        // §3.2 first: contact with the opposite liquid in any direction.
        let neighbors = [
            (x.wrapping_sub(1), y),
            (x + 1, y),
            (x, y.wrapping_sub(1)),
            (x, y + 1),
        ];
        for (nx, ny) in neighbors {
            if !world.in_bounds(nx, ny) {
                continue;
            }
            let other = world.tile(nx, ny).liquid.kind();
            if other.is_some() && other != Some(kind) {
                let ((wx, wy), (lx, ly)) = match kind {
                    LiquidKind::Water => ((x, y), (nx, ny)),
                    LiquidKind::Lava => ((nx, ny), (x, y)),
                };
                obsidianize(world, (wx, wy), (lx, ly), changed);
                // Both contact cells changed, so wake *their* full
                // neighborhoods — the caller only re-marks around the
                // visited cell, which leaves e.g. the water cell's other
                // neighbors frozen mid-flow when we were visiting the lava
                // cell (settle() would return a non-fixed-point).
                for (cx, cy) in [(wx, wy), (lx, ly)] {
                    self.mark(cx, cy);
                    self.mark(cx.wrapping_sub(1), cy);
                    self.mark(cx + 1, cy);
                    self.mark(cx, cy.wrapping_sub(1));
                    self.mark(cx, cy + 1);
                }
                return true;
            }
        }

        // Rule 1: flow down.
        if y + 1 < world.height && !world.is_solid(x as i32, y as i32 + 1) {
            let below = world.tile(x, y + 1).liquid;
            let below_level = if below.kind() == Some(kind) {
                below.level()
            } else {
                0 // empty (opposite kind was handled above)
            };
            if below_level < LIQUID_MAX_LEVEL {
                let moved = level.min(LIQUID_MAX_LEVEL - below_level);
                set_level(world, x, y, kind, level - moved);
                set_level(world, x, y + 1, kind, below_level + moved);
                changed.push((x, y));
                changed.push((x, y + 1));
                return true;
            }
        }

        // Rule 2: equalize horizontally (threshold ≥2; see module docs).
        let mut level = level;
        let mut any = false;
        for nx in [x.wrapping_sub(1), x + 1] {
            if level < 2 || !world.in_bounds(nx, y) || world.is_solid(nx as i32, y as i32) {
                continue;
            }
            let n = world.tile(nx, y).liquid;
            let n_level = if n.kind() == Some(kind) {
                n.level()
            } else if n.is_none() {
                0
            } else {
                continue; // opposite kind: already handled
            };
            if level >= n_level + 2 {
                level -= 1;
                set_level(world, x, y, kind, level);
                set_level(world, nx, y, kind, n_level + 1);
                changed.push((x, y));
                changed.push((nx, y));
                any = true;
            }
        }
        any
    }
}

/// §3.2: lava cell → Obsidian tile, water cell loses 1 level.
fn obsidianize(
    world: &mut World,
    (wx, wy): (u32, u32),
    (lx, ly): (u32, u32),
    changed: &mut Vec<(u32, u32)>,
) {
    let mut lava_cell = world.tile(lx, ly);
    lava_cell.id = TileId::Obsidian;
    lava_cell.liquid = Liquid::NONE;
    lava_cell.state = 0;
    world.set_tile(lx, ly, lava_cell);

    let water = world.tile(wx, wy).liquid;
    set_level(
        world,
        wx,
        wy,
        LiquidKind::Water,
        water.level().saturating_sub(1),
    );
    changed.push((lx, ly));
    changed.push((wx, wy));
}

fn set_level(world: &mut World, x: u32, y: u32, kind: LiquidKind, level: u8) {
    let mut t = world.tile(x, y);
    t.liquid = if level == 0 {
        Liquid::NONE
    } else {
        Liquid::new(kind, level)
    };
    world.set_tile(x, y, t);
}

/// Runs the automaton to equilibrium (worldgen pass 7 tail). Returns the
/// number of rounds it took.
pub fn settle(world: &mut World) -> u32 {
    let mut sim = FluidSim::new(world);
    sim.seed_all(world);
    let mut changed = Vec::new();
    let mut rounds = 0;
    while sim.is_active() && rounds < MAX_SETTLE_ROUNDS {
        changed.clear();
        sim.step(world, true, true, &mut changed);
        rounds += 1;
    }
    rounds
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferraria_shared::rng::Pcg32;
    use ferraria_shared::tiles::Tile;

    fn boxed_world(w: u32, h: u32) -> World {
        // A world whose outer border is stone, interior air.
        let mut world = World::new(w, h);
        for x in 0..w {
            world.set_tile(x, h - 1, Tile::of(TileId::Stone));
            world.set_tile(x, 0, Tile::of(TileId::Stone));
        }
        for y in 0..h {
            world.set_tile(0, y, Tile::of(TileId::Stone));
            world.set_tile(w - 1, y, Tile::of(TileId::Stone));
        }
        world
    }

    fn put(world: &mut World, x: u32, y: u32, kind: LiquidKind, level: u8) {
        let mut t = world.tile(x, y);
        t.liquid = Liquid::new(kind, level);
        world.set_tile(x, y, t);
    }

    fn total_level(world: &World, kind: LiquidKind) -> u32 {
        world
            .tiles
            .iter()
            .filter(|t| t.liquid.kind() == Some(kind))
            .map(|t| t.liquid.level() as u32)
            .sum()
    }

    #[test]
    fn water_falls_and_spreads_flat() {
        let mut w = boxed_world(12, 8);
        put(&mut w, 5, 1, LiquidKind::Water, 8);
        put(&mut w, 5, 2, LiquidKind::Water, 8);
        let rounds = settle(&mut w);
        assert!(rounds < MAX_SETTLE_ROUNDS);
        // Volume conserved.
        assert_eq!(total_level(&w, LiquidKind::Water), 16);
        // Everything ends on the floor row (y = 6) or one above it, and no
        // column holds liquid floating over an emptier cell.
        for y in 1..7 {
            for x in 1..11 {
                let l = w.tile(x, y).liquid;
                if l.is_some() {
                    let below = w.tile(x, y + 1);
                    assert!(
                        below.is_solid() || below.liquid.level() == 8,
                        "floating liquid at ({x},{y})"
                    );
                }
            }
        }
        // Settled water is equalized: adjacent levels differ by at most 1.
        for x in 1..10 {
            let a = w.tile(x, 6).liquid.level() as i32;
            let b = w.tile(x + 1, 6).liquid.level() as i32;
            assert!((a - b).abs() <= 1, "unequalized at x={x}: {a} vs {b}");
        }
    }

    #[test]
    fn water_onto_lava_makes_obsidian() {
        let mut w = boxed_world(6, 6);
        put(&mut w, 2, 4, LiquidKind::Lava, 8);
        put(&mut w, 2, 2, LiquidKind::Water, 8);
        settle(&mut w);
        // The contacted lava cell became obsidian and holds no liquid. (The
        // lava also spread sideways before the water landed, so its spill
        // cells convert too as the water equalizes over them.)
        assert_eq!(w.tile(2, 4).id, TileId::Obsidian);
        assert_eq!(w.tile(2, 4).liquid, Liquid::NONE);
        assert_eq!(total_level(&w, LiquidKind::Lava), 0);
        // §3.2: every conversion costs the water exactly 1 level.
        let obsidian = w.tiles.iter().filter(|t| t.id == TileId::Obsidian).count() as u32;
        assert!(obsidian >= 1);
        assert_eq!(total_level(&w, LiquidKind::Water), 8 - obsidian);
        assert_no_contact(&w);
    }

    #[test]
    fn lava_onto_water_also_converts_the_lava_cell() {
        let mut w = boxed_world(6, 7);
        put(&mut w, 2, 5, LiquidKind::Water, 8);
        put(&mut w, 2, 2, LiquidKind::Lava, 8);
        settle(&mut w);
        // Lava fell next to the water and converted in place.
        let obsidian = w.tiles.iter().filter(|t| t.id == TileId::Obsidian).count();
        assert!(obsidian >= 1, "no obsidian formed");
        assert_eq!(total_level(&w, LiquidKind::Lava), 0);
        assert!(total_level(&w, LiquidKind::Water) < 8);
        // Never water directly adjacent to lava once settled.
        assert_no_contact(&w);
    }

    #[test]
    fn settled_world_has_no_water_lava_contact() {
        let mut w = boxed_world(20, 10);
        // Interleave pools.
        for x in 2..8 {
            put(&mut w, x, 2, LiquidKind::Water, 8);
        }
        for x in 6..12 {
            put(&mut w, x, 5, LiquidKind::Lava, 8);
        }
        settle(&mut w);
        assert_no_contact(&w);
    }

    fn assert_no_contact(w: &World) {
        for y in 0..w.height {
            for x in 0..w.width {
                if w.tile(x, y).liquid.kind() == Some(LiquidKind::Water) {
                    for (nx, ny) in [(x + 1, y), (x, y + 1)] {
                        assert_ne!(
                            w.tile(nx, ny).liquid.kind(),
                            Some(LiquidKind::Lava),
                            "water at ({x},{y}) touches lava"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn settle_reaches_a_fixed_point_on_random_terrain() {
        // Regression: the §3.2 contact branch used to wake only the two
        // contact cells plus the *visited* cell's neighborhood. When the
        // visited cell was the lava cell, the water cell's other neighbors
        // never re-marked — e.g. a stable 2|1 water pair over lava froze in
        // a non-equilibrium state after the obsidian conversion drained the
        // level-1 cell. settle() must return a true fixed point.
        for seed in 0..30u64 {
            let mut w = boxed_world(48, 48);
            let mut rng = Pcg32::new(0xf00d ^ seed);
            for y in 1..47 {
                for x in 1..47 {
                    if rng.chance(0.35) {
                        w.set_tile(x, y, Tile::of(TileId::Stone));
                    } else {
                        match rng.gen_range_u32(0..10) {
                            0..=2 => {
                                let lvl = rng.gen_range_u32(1..9) as u8;
                                put(&mut w, x, y, LiquidKind::Water, lvl);
                            }
                            3 => {
                                let lvl = rng.gen_range_u32(1..9) as u8;
                                put(&mut w, x, y, LiquidKind::Lava, lvl);
                            }
                            _ => {}
                        }
                    }
                }
            }
            let rounds = settle(&mut w);
            assert!(rounds < MAX_SETTLE_ROUNDS, "seed {seed}: never terminated");
            assert_no_contact(&w);
            // Idempotency: a second settle() must change nothing at all.
            let snapshot = w.tiles.clone();
            settle(&mut w);
            assert_eq!(snapshot, w.tiles, "seed {seed}: settle not a fixed point");
        }
    }

    #[test]
    fn full_pool_is_stable() {
        let mut w = boxed_world(8, 6);
        for y in 2..5 {
            for x in 1..7 {
                put(&mut w, x, y, LiquidKind::Water, 8);
            }
        }
        let before = w.tiles.clone();
        let rounds = settle(&mut w);
        assert!(rounds <= 2, "flat-full pool should settle instantly");
        assert_eq!(before, w.tiles);
    }
}
