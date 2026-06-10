//! Live world ticks: the §3 fluid cadence, §2 sand gravity, the random-tick
//! system (grass spread / die-when-covered, torch extinguishing), sapling →
//! tree growth, and level-1 puddle evaporation.
//!
//! High-volume changes from these systems go through `Sim::stage_tile` and
//! flush once per tick as a batched [`ServerMessage::TilesChanged`]
//! (`ServerMessage` is `ferraria_shared::protocol`'s); player-driven changes
//! keep the immediate single-cell `TileChanged` path.

use ferraria_shared::tiles::{
    Liquid, LiquidKind, TileId, GRASS_SPREAD_DENOM, LAVA_UPDATE_TICKS, PUDDLE_EVAPORATE_SECS,
    SAPLING_AIR_NEEDED, TREE_HEIGHT_MAX, TREE_HEIGHT_MIN, WATER_UPDATE_TICKS,
};
use ferraria_shared::TICK_RATE;

use crate::worldgen::plant_tree;

use super::game::Sim;

/// Slow housekeeping (sapling growth checks, puddle evaporation, damage-map
/// purging) runs once per real second.
const HOUSEKEEPING_TICKS: u64 = TICK_RATE as u64;

impl Sim {
    /// One tick of every live world system.
    pub(crate) fn world_tick(&mut self) {
        self.step_fluids();
        self.step_sand();
        self.random_ticks();
        if self.tick.is_multiple_of(HOUSEKEEPING_TICKS) {
            self.grow_saplings();
            self.evaporate_puddles();
            self.purge_stale_damage();
        }
    }

    /// §3 cadence: water cells update every 2 ticks, lava every 5. Changed
    /// cells are staged for the batched broadcast, then post-processed
    /// (torch extinguishing, puddle tracking).
    fn step_fluids(&mut self) {
        let water = self.tick.is_multiple_of(WATER_UPDATE_TICKS as u64);
        let lava = self.tick.is_multiple_of(LAVA_UPDATE_TICKS as u64);
        if !water && !lava {
            return;
        }
        let mut changed = Vec::new();
        self.fluids.step(&mut self.world, water, lava, &mut changed);
        changed.sort_unstable();
        changed.dedup();
        for &(x, y) in &changed {
            self.stage_tile(x, y);
            self.track_puddle(x, y);
            let t = self.world.tile(x, y);
            // §2 tile 16: torches extinguish in water (the item pops out).
            if t.id == TileId::Torch && t.liquid.kind() == Some(LiquidKind::Water) {
                self.break_tile(x, y);
            }
            // Sand above a cell the liquid just vacated stays put (it only
            // falls through Air *foreground*), but sand resting in/over
            // water that drained may now have an empty cell below it.
            if t.id == TileId::Sand {
                self.sand_active.insert((x, y));
            }
        }
    }

    /// §2 tile 4: unsupported sand descends one cell per tick (instant tile
    /// descent), sinking through liquids (displacing them upward) and
    /// settling on any non-air foreground.
    fn step_sand(&mut self) {
        if self.sand_active.is_empty() {
            return;
        }
        let mut cells: Vec<(u32, u32)> = self.sand_active.drain().collect();
        // Bottom-up so a whole column moves together within one tick.
        cells.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (x, y) in cells {
            if self.world.tile(x, y).id != TileId::Sand {
                continue;
            }
            let below_y = y + 1;
            if below_y >= self.world.height {
                continue; // world floor supports it
            }
            let below = self.world.tile(x, below_y);
            if below.id != TileId::Air {
                continue; // settled on a solid (or any foreground object)
            }
            // Move down one cell, displacing the destination's liquid into
            // the vacated cell so volume is conserved.
            let mut src = self.world.tile(x, y);
            let displaced = below.liquid;
            let mut dst = below;
            dst.id = TileId::Sand;
            dst.liquid = Liquid::NONE;
            dst.state &= ferraria_shared::tiles::state::WALL_PLACED;
            src.id = TileId::Air;
            src.liquid = displaced;
            src.state &= ferraria_shared::tiles::state::WALL_PLACED;
            self.world.set_tile(x, y, src);
            self.world.set_tile(x, below_y, dst);
            self.stage_tile(x, y);
            self.stage_tile(x, below_y);
            self.fluids.mark(x, y);
            self.fluids.mark(x, y.wrapping_sub(1));
            self.fluids.mark(x, below_y + 1);
            self.fluids.mark(x.wrapping_sub(1), below_y);
            self.fluids.mark(x + 1, below_y);
            // Keep falling next tick; the sand that may rest on the vacated
            // cell falls too.
            self.sand_active.insert((x, below_y));
            if y > 0 && self.world.tile(x, y - 1).id == TileId::Sand {
                self.sand_active.insert((x, y - 1));
            }
        }
    }

    /// The random-tick system: each tile has a ~1/[`GRASS_SPREAD_DENOM`]
    /// chance per tick of receiving a tile update (DESIGN §2 tile 3), i.e.
    /// `area / 600` random cells visited per tick.
    fn random_ticks(&mut self) {
        let area = self.world.width as u64 * self.world.height as u64;
        let budget = (area / GRASS_SPREAD_DENOM as u64).max(1);
        for _ in 0..budget {
            let x = self.loot_rng.gen_range_u32(0..self.world.width);
            let y = self.loot_rng.gen_range_u32(0..self.world.height);
            self.random_tick_cell(x, y);
        }
    }

    /// One tile update at `(x, y)` (split out so tests can drive cells
    /// deterministically).
    pub(crate) fn random_tick_cell(&mut self, x: u32, y: u32) {
        let t = self.world.tile(x, y);
        match t.id {
            // §2 tile 3: grass dies when covered, else spreads to adjacent
            // air-exposed dirt.
            TileId::Grass => {
                if !air_exposed(&self.world, x, y) {
                    let mut t = t;
                    t.id = TileId::Dirt;
                    self.world.set_tile(x, y, t);
                    self.stage_tile(x, y);
                    return;
                }
                let (xi, yi) = (x as i32, y as i32);
                let candidates: Vec<(u32, u32)> = [
                    (xi - 1, yi - 1),
                    (xi, yi - 1),
                    (xi + 1, yi - 1),
                    (xi - 1, yi),
                    (xi + 1, yi),
                    (xi - 1, yi + 1),
                    (xi, yi + 1),
                    (xi + 1, yi + 1),
                ]
                .into_iter()
                .filter(|&(nx, ny)| nx >= 0 && ny >= 0)
                .map(|(nx, ny)| (nx as u32, ny as u32))
                .filter(|&(nx, ny)| {
                    self.world.in_bounds(nx, ny)
                        && self.world.tile(nx, ny).id == TileId::Dirt
                        && air_exposed(&self.world, nx, ny)
                })
                .collect();
                if let Some(&(nx, ny)) = self.loot_rng.pick(&candidates) {
                    let mut n = self.world.tile(nx, ny);
                    n.id = TileId::Grass;
                    self.world.set_tile(nx, ny, n);
                    self.stage_tile(nx, ny);
                }
            }
            // Safety net for torches that ended up under water through any
            // path the fluid post-processing didn't see.
            TileId::Torch if t.liquid.kind() == Some(LiquidKind::Water) => {
                self.break_tile(x, y);
            }
            _ => {}
        }
    }

    /// §2 tile 31: due saplings become 7–16-segment trees when 7+ air tiles
    /// stand above them; blocked saplings retry every second.
    fn grow_saplings(&mut self) {
        let due: Vec<(u32, u32)> = self
            .saplings
            .iter()
            .filter(|&(_, &at)| self.tick >= at)
            .map(|(&pos, _)| pos)
            .collect();
        for (x, y) in due {
            if self.world.tile(x, y).id != TileId::Sapling {
                self.saplings.remove(&(x, y)); // broken/overwritten meanwhile
                continue;
            }
            let mut air_above = 0u32;
            while air_above < TREE_HEIGHT_MAX
                && y > air_above
                && self.world.is_empty(x, y - air_above - 1)
            {
                air_above += 1;
            }
            if air_above < SAPLING_AIR_NEEDED {
                continue; // §2: needs 7+ air tiles above; retry later
            }
            let height = self
                .loot_rng
                .gen_range_u32(TREE_HEIGHT_MIN..TREE_HEIGHT_MAX + 1)
                .min(air_above + 1);
            // The lowest trunk segment replaces the sapling cell itself.
            let mut cleared = self.world.tile(x, y);
            cleared.id = TileId::Air;
            self.world.set_tile(x, y, cleared);
            if plant_tree(&mut self.world, x, y + 1, height) {
                self.saplings.remove(&(x, y));
                for i in 1..=height {
                    self.stage_tile(x, y + 1 - i);
                }
            } else {
                // Couldn't grow after all; put the sapling back.
                let mut t = self.world.tile(x, y);
                t.id = TileId::Sapling;
                self.world.set_tile(x, y, t);
            }
        }
    }

    /// §3: level-1 puddles on flat ground evaporate after 60 s.
    fn evaporate_puddles(&mut self) {
        let ttl = (PUDDLE_EVAPORATE_SECS * TICK_RATE as f32) as u64;
        let due: Vec<((u32, u32), u64)> = self
            .puddles
            .iter()
            .map(|(&pos, &since)| (pos, since))
            .collect();
        for ((x, y), since) in due {
            if !is_puddle(&self.world, x, y) {
                self.puddles.remove(&(x, y));
                continue;
            }
            if self.tick.saturating_sub(since) >= ttl {
                self.puddles.remove(&(x, y));
                let mut t = self.world.tile(x, y);
                t.liquid = Liquid::NONE;
                self.world.set_tile(x, y, t);
                self.stage_tile(x, y);
                self.fluids.mark(x, y);
                self.fluids.mark(x.wrapping_sub(1), y);
                self.fluids.mark(x + 1, y);
                self.fluids.mark(x, y.wrapping_sub(1));
            }
        }
    }

    /// Starts/stops tracking `(x, y)` as an evaporation candidate.
    pub(crate) fn track_puddle(&mut self, x: u32, y: u32) {
        if !self.world.in_bounds(x, y) {
            return;
        }
        if is_puddle(&self.world, x, y) {
            let tick = self.tick;
            self.puddles.entry((x, y)).or_insert(tick);
        } else {
            self.puddles.remove(&(x, y));
        }
    }

    /// Seeds puddle tracking from the generated world (settled pools leave
    /// level-1 shores that are already due to dry up).
    pub(crate) fn scan_initial_puddles(&mut self) {
        for y in 0..self.world.height {
            for x in 0..self.world.width {
                if is_puddle(&self.world, x, y) {
                    self.puddles.insert((x, y), 0);
                }
            }
        }
    }

    /// Drops mining-damage entries whose §2 5 s decay long passed, so the
    /// map stays bounded.
    fn purge_stale_damage(&mut self) {
        let reset_ticks =
            (ferraria_shared::tiles::TILE_DAMAGE_RESET_SECS * TICK_RATE as f32) as u64;
        let tick = self.tick;
        self.tile_damage
            .retain(|_, d| tick.saturating_sub(d.last_hit_tick) <= reset_ticks);
    }
}

/// Has at least one non-solid cardinal neighbor (the §2 grass exposure test;
/// world borders don't count as exposure).
fn air_exposed(world: &ferraria_shared::world::World, x: u32, y: u32) -> bool {
    let (xi, yi) = (x as i32, y as i32);
    [(xi - 1, yi), (xi + 1, yi), (xi, yi - 1), (xi, yi + 1)]
        .into_iter()
        .any(|(nx, ny)| !world.is_solid(nx, ny))
}

/// A level-1 liquid cell resting directly on solid ground.
fn is_puddle(world: &ferraria_shared::world::World, x: u32, y: u32) -> bool {
    let t = world.tile(x, y);
    t.liquid.is_some() && t.liquid.level() == 1 && world.is_solid(x as i32, y as i32 + 1)
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;
    use ferraria_shared::protocol::ServerMessage;
    use ferraria_shared::tiles::Tile;

    const FLOOR: u32 = 30;

    fn set_tile(sim: &mut super::super::game::Sim, x: u32, y: u32, id: TileId) {
        let mut t = sim.world().tile(x, y);
        t.id = id;
        sim.change_tile(x, y, t);
    }

    #[test]
    fn fluids_step_on_cadence_and_broadcast_batches() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let (_id, _epoch, mut rx) = join(&mut sim, "alice");
        drain(&mut rx);
        // A 1-wide stone cup on the floor keeps the water from spreading.
        let (x, y) = (52, FLOOR - 3);
        set_tile(&mut sim, x - 1, FLOOR - 1, TileId::Stone);
        set_tile(&mut sim, x + 1, FLOOR - 1, TileId::Stone);
        // A full water cell two above the cup: it must flow down.
        let mut t = Tile::AIR;
        t.liquid = Liquid::new(LiquidKind::Water, 8);
        sim.change_tile(x, y, t);
        drain(&mut rx);

        // Tick 1 is off-cadence for water (every 2 ticks): nothing moves.
        advance(&mut sim, 1);
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ServerMessage::TilesChanged { .. })),
            "no fluid batch off-cadence"
        );
        assert_eq!(sim.world().tile(x, y).liquid.level(), 8);

        // Tick 2: the water falls one cell, batched as TilesChanged.
        advance(&mut sim, 1);
        let msgs = drain(&mut rx);
        let batch = msgs
            .iter()
            .find_map(|m| match m {
                ServerMessage::TilesChanged { changes } => Some(changes.clone()),
                _ => None,
            })
            .expect("fluid batch broadcast");
        assert!(batch.iter().any(
            |&(bx, by, t)| (bx, by) == (x, y + 1) && t.liquid.kind() == Some(LiquidKind::Water)
        ));
        assert_eq!(sim.world().tile(x, y).liquid.level(), 0);
        assert_eq!(sim.world().tile(x, y + 1).liquid.level(), 8);

        // Settles on the floor and goes quiet.
        advance(&mut sim, 10);
        assert_eq!(sim.world().tile(x, FLOOR - 1).liquid.level(), 8);
        drain(&mut rx);
        advance(&mut sim, 10);
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ServerMessage::TilesChanged { .. })),
            "settled water stops broadcasting"
        );
    }

    #[test]
    fn sand_columns_collapse_one_cell_per_tick() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let (_id, _epoch, mut rx) = join(&mut sim, "alice");
        drain(&mut rx);
        // A stone shelf 3 cells above the floor holds a 3-sand column.
        let x = 52;
        let shelf = FLOOR - 3;
        set_tile(&mut sim, x, shelf, TileId::Stone);
        for dy in 1..=3 {
            set_tile(&mut sim, x, shelf - dy, TileId::Sand);
        }
        drain(&mut rx);
        // Knock out the shelf: the column descends 1 cell per tick, moving
        // as a unit, until it rests on the floor.
        set_tile(&mut sim, x, shelf, TileId::Air);

        advance(&mut sim, 1);
        assert_eq!(sim.world().tile(x, shelf).id, TileId::Sand, "fell 1");
        assert_eq!(sim.world().tile(x, shelf - 1).id, TileId::Sand);
        assert_eq!(
            sim.world().tile(x, shelf - 3).id,
            TileId::Air,
            "top vacated"
        );
        assert!(
            drain(&mut rx)
                .iter()
                .any(|m| matches!(m, ServerMessage::TilesChanged { .. })),
            "sand movement batches"
        );

        advance(&mut sim, 1);
        assert_eq!(sim.world().tile(x, shelf + 1).id, TileId::Sand, "fell 2");

        advance(&mut sim, 6);
        // Settled: 3 sand resting on the floor, nothing floating above.
        for dy in 1..=3 {
            assert_eq!(sim.world().tile(x, FLOOR - dy).id, TileId::Sand);
        }
        assert_eq!(sim.world().tile(x, FLOOR - 4).id, TileId::Air);
        let total = (0..FLOOR)
            .filter(|&y| sim.world().tile(x, y).id == TileId::Sand)
            .count();
        assert_eq!(total, 3, "sand conserved");
    }

    #[test]
    fn sand_sinks_through_water_displacing_it() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let x = 52;
        // Water resting on the floor with sand floating right above it.
        let mut w = Tile::AIR;
        w.liquid = Liquid::new(LiquidKind::Water, 8);
        sim.change_tile(x, FLOOR - 1, w);
        set_tile(&mut sim, x, FLOOR - 2, TileId::Sand);

        advance(&mut sim, 1);
        assert_eq!(sim.world().tile(x, FLOOR - 1).id, TileId::Sand, "sank");
        assert_eq!(
            sim.world().tile(x, FLOOR - 1).liquid.level(),
            0,
            "solid cell holds no liquid"
        );
        assert_eq!(
            sim.world().tile(x, FLOOR - 2).liquid.level(),
            8,
            "water displaced upward"
        );
    }

    #[test]
    fn grass_spreads_and_dies_by_random_tick() {
        let mut sim = flat_sim(100, 60, FLOOR);
        // Grass next to exposed dirt: a tile update converts the dirt.
        let x = 52;
        set_tile(&mut sim, x, FLOOR, TileId::Grass);
        set_tile(&mut sim, x + 1, FLOOR, TileId::Dirt);
        sim.random_tick_cell(x, FLOOR);
        assert_eq!(sim.world().tile(x + 1, FLOOR).id, TileId::Grass, "spread");

        // Covered grass (all four neighbors solid) dies to dirt.
        let (cx, cy) = (60, FLOOR + 2); // inside the floor slab
        set_tile(&mut sim, cx, cy, TileId::Grass);
        sim.random_tick_cell(cx, cy);
        assert_eq!(sim.world().tile(cx, cy).id, TileId::Dirt, "died covered");
    }

    #[test]
    fn torches_extinguish_under_water() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let (x, y) = (52, FLOOR - 1);
        let mut t = Tile::of(TileId::Torch);
        t.liquid = Liquid::new(LiquidKind::Water, 4);
        sim.change_tile(x, y, t);
        sim.random_tick_cell(x, y);
        assert_eq!(sim.world().tile(x, y).id, TileId::Air, "snuffed out");
        // The torch pops out as a drop.
        assert_eq!(sim.entities.map.len(), 1);
    }

    #[test]
    fn saplings_grow_into_trees_when_unblocked() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let x = 52;
        set_tile(&mut sim, x, FLOOR, TileId::Grass);
        set_tile(&mut sim, x, FLOOR - 1, TileId::Sapling);
        sim.saplings.insert((x, FLOOR - 1), 0); // due immediately

        // Blocked: a ceiling 3 tiles up leaves < 7 air above.
        set_tile(&mut sim, x, FLOOR - 4, TileId::Stone);
        advance(&mut sim, 61);
        assert_eq!(sim.world().tile(x, FLOOR - 1).id, TileId::Sapling);
        assert!(sim.saplings.contains_key(&(x, FLOOR - 1)), "retries later");

        // Unblock: it grows into a 7–16 segment tree with a crown.
        set_tile(&mut sim, x, FLOOR - 4, TileId::Air);
        advance(&mut sim, 61);
        assert!(!sim.saplings.contains_key(&(x, FLOOR - 1)));
        let mut height = 0;
        while sim.world().tile(x, FLOOR - 1 - height).id == TileId::TreeTrunk {
            height += 1;
        }
        assert!(
            (TREE_HEIGHT_MIN..=TREE_HEIGHT_MAX).contains(&height),
            "tree height {height}"
        );
        let top = sim.world().tile(x, FLOOR - height);
        assert_eq!(
            top.state & 0x7,
            ferraria_shared::tiles::state::TREE_SEGMENT_TOP
        );
    }

    #[test]
    fn level_one_puddles_evaporate_after_a_minute() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let (x, y) = (52, FLOOR - 1);
        let mut t = Tile::AIR;
        t.liquid = Liquid::new(LiquidKind::Water, 1);
        sim.change_tile(x, y, t);
        assert!(sim.puddles.contains_key(&(x, y)), "tracked on change");

        // 59 s: still wet. (The fluid automaton leaves a stable level-1
        // puddle alone.)
        advance(&mut sim, 59 * 60);
        assert_eq!(sim.world().tile(x, y).liquid.level(), 1);
        // Past 60 s (plus the once-a-second sweep): gone.
        advance(&mut sim, 2 * 60);
        assert_eq!(sim.world().tile(x, y).liquid.level(), 0);
        assert!(!sim.puddles.contains_key(&(x, y)));
    }
}
