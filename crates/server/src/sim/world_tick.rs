//! Live world ticks: the §3 fluid cadence, §2 sand gravity, the random-tick
//! system (grass spread / die-when-covered, torch extinguishing), sapling →
//! tree growth, and level-1 puddle evaporation.
//!
//! High-volume changes from these systems go through `Sim::stage_tile` and
//! flush once per tick as a batched [`ServerMessage::TilesChanged`]
//! (`ServerMessage` is `ferraria_shared::protocol`'s); player-driven changes
//! keep the immediate single-cell `TileChanged` path.

use ferraria_shared::items::ItemId;
use ferraria_shared::physics::{step_falling_tile, COLLISION_EPS, PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::DespawnReason;
use ferraria_shared::tiles::{
    state, Liquid, LiquidKind, TileId, GRASS_SPREAD_DENOM, LAVA_UPDATE_TICKS,
    PUDDLE_EVAPORATE_SECS, SAPLING_AIR_NEEDED, TREE_HEIGHT_MAX, TREE_HEIGHT_MIN,
    WATER_UPDATE_TICKS,
};
use ferraria_shared::{DT, TICK_RATE};

use crate::worldgen::plant_tree;

use super::entities::EntityKind;
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

    /// §2 tile 4: a sand cell with no **solid** tile below leaves the grid
    /// and becomes a falling-sand entity (stepped by
    /// [`Sim::step_falling_sand`], 37.5 t/s terminal cap). Non-solid
    /// fixtures (torches, pots, open doors, ...) do not support sand.
    fn step_sand(&mut self) {
        if self.sand_active.is_empty() {
            return;
        }
        let mut cells: Vec<(u32, u32)> = self.sand_active.drain().collect();
        // Bottom-up so a whole column launches in order.
        cells.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (x, y) in cells {
            if self.world.tile(x, y).id != TileId::Sand {
                continue;
            }
            if y + 1 >= self.world.height {
                continue; // world floor supports it
            }
            if self.world.is_solid(x as i32, y as i32 + 1) {
                continue; // §2: only a solid tile below supports sand
            }
            let mut t = self.world.tile(x, y);
            t.id = TileId::Air;
            t.state &= state::WALL_PLACED;
            self.world.set_tile(x, y, t);
            self.stage_tile(x, y);
            // Partial mining cracks don't follow the entity.
            self.tile_damage.remove(&(x, y, false));
            self.fluids.mark(x, y);
            self.fluids.mark(x.wrapping_sub(1), y);
            self.fluids.mark(x + 1, y);
            self.fluids.mark(x, y.wrapping_sub(1));
            self.fluids.mark(x, y + 1);
            self.queue_support_checks(x, y);
            self.spawn_falling_sand(x, y);
            // The sand resting on the vacated cell follows next tick.
            if y > 0 && self.world.tile(x, y - 1).id == TileId::Sand {
                self.sand_active.insert((x, y - 1));
            }
        }
    }

    /// Integrates falling-sand entities (§2 tile 4): gravity under the §0
    /// terminal cap and §3 liquid multipliers, colliding with solids only.
    /// A landed entity converts back into a sand tile in the cell it rests
    /// in; see [`Sim::settle_falling_sand`] for the occupied-cell rules.
    pub(crate) fn step_falling_sand(&mut self) {
        let falling: Vec<u32> = self
            .entities
            .map
            .iter()
            .filter(|(_, e)| matches!(e.kind, EntityKind::FallingSand))
            .map(|(&id, _)| id)
            .collect();
        for id in falling {
            let Some(e) = self.entities.map.get(&id) else {
                continue;
            };
            let (mut pos, mut vel) = (e.pos, e.vel);
            let landed = step_falling_tile(&self.world, &mut pos, &mut vel, DT);
            if let Some(e) = self.entities.map.get_mut(&id) {
                e.pos = pos;
                e.vel = vel;
                e.awake = true;
            }
            if landed {
                self.settle_falling_sand(id);
            }
        }
    }

    /// Converts a landed falling-sand entity back into a tile. A lone
    /// one-hit 1×1 fixture in the landing cell (torch, pot, cobweb,
    /// sapling, ...) pops first, dropping its item/loot; anything sturdier
    /// (multi-tile furniture, tree trunks, open doors) survives and the sand
    /// becomes an item drop instead — as it also does when solidifying
    /// would bury a player. The landing cell's liquid is displaced into the
    /// cell above where there is room.
    fn settle_falling_sand(&mut self, id: u32) {
        let Some(e) = self.entities.map.get(&id) else {
            return;
        };
        // The entity never moves horizontally and landing clamps its top
        // edge just above the supporting row, so rounding recovers the cell.
        let (cx, cy) = (e.pos.0 + 0.5, e.pos.1 + 0.5);
        if cx < 0.0 || cy < 0.0 {
            self.despawn_entity(id, DespawnReason::Despawned);
            return;
        }
        let (x, y) = (cx as u32, cy as u32);
        if !self.world.in_bounds(x, y) {
            self.despawn_entity(id, DespawnReason::Despawned);
            return;
        }
        let center = (x as f32 + 0.5, y as f32 + 0.5);
        let target = self.world.tile(x, y);
        let data = target.id.data();
        if target.id != TileId::Air && data.one_hit && data.size == (1, 1) && data.breakable {
            self.break_tile(x, y);
        }
        let occupied = self.world.tile(x, y).id != TileId::Air;
        if occupied || self.sand_would_bury_player(x, y) {
            self.despawn_entity(id, DespawnReason::Despawned);
            self.spawn_item_drop(ItemId::Sand, 1, center);
            return;
        }
        let mut t = self.world.tile(x, y);
        let displaced = t.liquid;
        t.id = TileId::Sand;
        t.liquid = Liquid::NONE;
        t.state &= state::WALL_PLACED;
        self.despawn_entity(id, DespawnReason::Despawned);
        self.change_tile(x, y, t);
        if displaced.is_some() && y > 0 {
            let mut above = self.world.tile(x, y - 1);
            if !above.is_solid() && above.liquid.is_none() {
                above.liquid = displaced;
                self.change_tile(x, y - 1, above);
            }
            // Else the volume is lost, matching how placed solids displace
            // (destroy) liquid (§2 placement).
        }
    }

    /// Whether solidifying a sand tile at `(x, y)` would overlap a player's
    /// hitbox (shrunk by the collision skin so flush contacts don't count).
    fn sand_would_bury_player(&self, x: u32, y: u32) -> bool {
        let (rx0, ry0) = (x as f32 + COLLISION_EPS, y as f32 + COLLISION_EPS);
        let (rx1, ry1) = (
            (x + 1) as f32 - COLLISION_EPS,
            (y + 1) as f32 - COLLISION_EPS,
        );
        self.players.values().any(|p| {
            p.pos.0 < rx1
                && p.pos.0 + PLAYER_WIDTH > rx0
                && p.pos.1 < ry1
                && p.pos.1 + PLAYER_HEIGHT > ry0
        })
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
                    // A buried forage plant dies with the grass.
                    t.state &= !state::VARIANT_MASK;
                    self.world.set_tile(x, y, t);
                    self.stage_tile(x, y);
                    // The cell's foreground changed: mining cracks on the
                    // grass must not carry over to the dirt.
                    self.tile_damage.remove(&(x, y, false));
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
                    self.tile_damage.remove(&(nx, ny, false));
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
                    self.tile_damage.remove(&(x, y + 1 - i, false));
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

    /// Runs ticks until no falling-sand entity remains (or panics).
    fn settle_sand(sim: &mut super::super::game::Sim, max_ticks: u32) {
        for _ in 0..max_ticks {
            advance(sim, 1);
            let falling = sim
                .entities
                .map
                .values()
                .any(|e| matches!(e.kind, EntityKind::FallingSand));
            if !falling {
                return;
            }
        }
        panic!("falling sand never settled within {max_ticks} ticks");
    }

    #[test]
    fn unsupported_sand_columns_fall_as_entities_and_resettle() {
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
        // Knock out the shelf: the column converts to falling entities (§2
        // tile 4: "becomes falling entity").
        set_tile(&mut sim, x, shelf, TileId::Air);

        advance(&mut sim, 1);
        assert_eq!(
            sim.world().tile(x, shelf - 1).id,
            TileId::Air,
            "bottom sand left the grid"
        );
        assert!(
            sim.entities
                .map
                .values()
                .any(|e| matches!(e.kind, EntityKind::FallingSand)),
            "a falling-sand entity exists"
        );
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerMessage::EntitySpawn {
                    kind: ferraria_shared::protocol::EntityKind::FallingSand,
                    ..
                }
            )),
            "falling sand announces itself as an entity: {msgs:?}"
        );

        settle_sand(&mut sim, 120);
        // Settled: 3 sand resting on the floor, nothing floating above.
        for dy in 1..=3 {
            assert_eq!(sim.world().tile(x, FLOOR - dy).id, TileId::Sand);
        }
        assert_eq!(sim.world().tile(x, FLOOR - 4).id, TileId::Air);
        let total = (0..FLOOR)
            .filter(|&y| sim.world().tile(x, y).id == TileId::Sand)
            .count();
        assert_eq!(total, 3, "sand conserved");
        // The settles were broadcast (immediate TileChanged per landing).
        assert!(drain(&mut rx).iter().any(|m| matches!(m,
            ServerMessage::TileChanged { tile, .. } if tile.id == TileId::Sand)));
    }

    #[test]
    fn falling_sand_respects_the_terminal_velocity_cap() {
        // §2 tile 4: 37.5 t/s cap. A long free fall must never exceed it
        // (the old tile-stepped descent moved 60 t/s).
        let floor = 180;
        let mut sim = flat_sim(60, 200, floor);
        set_tile(&mut sim, 30, 20, TileId::Sand);
        let mut max_vel: f32 = 0.0;
        for _ in 0..500 {
            advance(&mut sim, 1);
            let mut falling = false;
            for e in sim.entities.map.values() {
                if matches!(e.kind, EntityKind::FallingSand) {
                    falling = true;
                    max_vel = max_vel.max(e.vel.1);
                }
            }
            if !falling && sim.world().tile(30, floor - 1).id == TileId::Sand {
                break;
            }
        }
        assert_eq!(sim.world().tile(30, floor - 1).id, TileId::Sand, "landed");
        assert!(
            max_vel > ferraria_shared::TERMINAL_VELOCITY * 0.9,
            "long fall reaches terminal velocity, peaked at {max_vel}"
        );
        assert!(
            max_vel <= ferraria_shared::TERMINAL_VELOCITY + 1e-3,
            "§0 cap exceeded: {max_vel}"
        );
    }

    #[test]
    fn sand_sinks_through_water_displacing_it() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let x = 52;
        // A 1-wide stone cup so the water can't equalize away while the
        // sand is in flight.
        set_tile(&mut sim, x - 1, FLOOR - 1, TileId::Stone);
        set_tile(&mut sim, x + 1, FLOOR - 1, TileId::Stone);
        // Water resting on the floor with sand floating right above it.
        let mut w = Tile::AIR;
        w.liquid = Liquid::new(LiquidKind::Water, 8);
        sim.change_tile(x, FLOOR - 1, w);
        set_tile(&mut sim, x, FLOOR - 2, TileId::Sand);

        settle_sand(&mut sim, 120);
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
    fn sand_does_not_rest_on_fixtures_and_pops_one_hit_ones() {
        // §2 row 4 says sand falls "if no solid tile below" — torches, pots
        // and other non-solid fixtures are not support. A 1×1 one-hit
        // fixture in the landing cell pops (dropping its item) and the sand
        // settles in its place.
        let mut sim = flat_sim(100, 60, FLOOR);
        let x = 60;
        set_tile(&mut sim, x, FLOOR - 1, TileId::Torch);
        set_tile(&mut sim, x, FLOOR - 3, TileId::Sand);

        settle_sand(&mut sim, 120);
        assert_eq!(
            sim.world().tile(x, FLOOR - 1).id,
            TileId::Sand,
            "sand landed where the torch stood"
        );
        let torch_drops: u32 = sim
            .entities
            .map
            .values()
            .filter_map(|e| match e.kind {
                EntityKind::ItemDrop {
                    item: ferraria_shared::items::ItemId::Torch,
                    count,
                } => Some(count as u32),
                _ => None,
            })
            .sum();
        assert_eq!(torch_drops, 1, "the popped torch dropped its item");
    }

    #[test]
    fn sand_blocked_by_furniture_becomes_an_item_drop() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let x = 60;
        // A chest (2×2, not one-hit-poppable by sand) on the floor.
        assert!(sim.world.place_multitile(x, FLOOR - 2, TileId::Chest));
        set_tile(&mut sim, x, FLOOR - 3, TileId::Sand);

        settle_sand(&mut sim, 120);
        assert_eq!(sim.world().tile(x, FLOOR - 2).id, TileId::Chest, "intact");
        assert_eq!(sim.world().tile(x, FLOOR - 1).id, TileId::Chest, "intact");
        let sand_drops: u32 = sim
            .entities
            .map
            .values()
            .filter_map(|e| match e.kind {
                EntityKind::ItemDrop {
                    item: ferraria_shared::items::ItemId::Sand,
                    count,
                } => Some(count as u32),
                _ => None,
            })
            .sum();
        assert_eq!(sand_drops, 1, "the sand became an item instead");
    }

    #[test]
    fn sand_never_solidifies_inside_a_player() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let (id, _epoch, mut rx) = join(&mut sim, "alice");
        drain(&mut rx);
        place_player(&mut sim, id, 60.5, FLOOR as f32);
        // Sand dropped straight onto the player's column.
        set_tile(&mut sim, 60, FLOOR - 6, TileId::Sand);

        settle_sand(&mut sim, 120);
        for dy in 1..=6 {
            assert_ne!(
                sim.world().tile(60, FLOOR - dy).id,
                TileId::Sand,
                "no sand tile materialized in the player's column (row -{dy})"
            );
        }
        // The sand survives as an item: still a drop, or already picked up.
        let dropped: u32 = sim
            .entities
            .map
            .values()
            .filter_map(|e| match e.kind {
                EntityKind::ItemDrop {
                    item: ferraria_shared::items::ItemId::Sand,
                    count,
                } => Some(count as u32),
                _ => None,
            })
            .sum();
        let carried: u32 = sim.players[&id]
            .inventory
            .iter()
            .flatten()
            .filter(|s| s.item == ferraria_shared::items::ItemId::Sand)
            .map(|s| s.count as u32)
            .sum();
        assert_eq!(dropped + carried, 1, "sand conserved as an item");
    }

    #[test]
    fn system_tile_changes_clear_mining_damage() {
        // Batched (stage_tile-path) foreground changes must not leave §2
        // break-points behind: sand that falls away used to bequeath its
        // cracks to whatever landed there next.
        let mut sim = flat_sim(100, 60, FLOOR);
        let (x, y) = (52, FLOOR - 3);
        set_tile(&mut sim, x, y + 1, TileId::Stone); // shelf
        set_tile(&mut sim, x, y, TileId::Sand);
        sim.tile_damage.insert(
            (x, y, false),
            super::super::interact::TileDamage {
                damage: 75.0,
                last_hit_tick: sim.tick,
            },
        );
        set_tile(&mut sim, x, y + 1, TileId::Air); // unsupport it
        advance(&mut sim, 1);
        assert_eq!(sim.world().tile(x, y).id, TileId::Air, "sand left");
        assert!(
            !sim.tile_damage.contains_key(&(x, y, false)),
            "stale damage purged when the system vacated the cell"
        );

        // Grass dying by random tick clears its cell's damage too.
        let (gx, gy) = (60, FLOOR + 2); // covered: inside the floor slab
        set_tile(&mut sim, gx, gy, TileId::Grass);
        sim.tile_damage.insert(
            (gx, gy, false),
            super::super::interact::TileDamage {
                damage: 75.0,
                last_hit_tick: sim.tick,
            },
        );
        sim.random_tick_cell(gx, gy);
        assert_eq!(sim.world().tile(gx, gy).id, TileId::Dirt);
        assert!(!sim.tile_damage.contains_key(&(gx, gy, false)));
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
        let drops = sim
            .entities
            .map
            .values()
            .filter(|e| matches!(e.kind, EntityKind::ItemDrop { .. }))
            .count();
        assert_eq!(drops, 1);
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
