//! Mine/place/build intent handlers (DESIGN §2, §8 reach): `HitTile`,
//! `HitWall`, `PlaceTile`, `PlaceWall`, `ToggleDoor`.
//!
//! The §2 mining model: every tile has 100 break-points; each accepted
//! swing deals `tool_power × hardness_mult` (zero below the tile's minimum
//! power); accumulated damage decays after 5 s without hits. Swings are
//! rate-limited server-side by the held tool's use time (±1 tick of network
//! jitter tolerated). All world mutation goes through `Sim::change_tile`
//! (immediate `TileChanged`), and all drops through `Sim::spawn_item_drop`.

use ferraria_shared::items::{inventory, ItemId, Placement, ToolStats, BARE_HAND_USE_SECS};
use ferraria_shared::physics::{COLLISION_EPS, PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::ServerMessage;
use ferraria_shared::tiles::{
    state, Liquid, LiquidKind, Solidity, TileId, ToolKind, WallId, ACORN_DROP_CHANCE,
    SAPLING_GROW_MAX_SECS, SAPLING_GROW_MIN_SECS, TILE_BREAK_POINTS, TILE_DAMAGE_RESET_SECS,
    WOOD_PER_TREE_SEGMENT,
};
use ferraria_shared::world::World;
use ferraria_shared::{tile_in_reach, DOOR_TOGGLE_COOLDOWN_TICKS, TICK_RATE};

use crate::worldgen::loot;

use super::game::Sim;

/// Accumulated §2 mining damage on one cell (foreground or wall layer).
#[derive(Debug, Clone, Copy)]
pub struct TileDamage {
    /// Break-points dealt so far (out of [`TILE_BREAK_POINTS`]).
    pub damage: f32,
    /// Tick of the last hit; the total resets after
    /// [`TILE_DAMAGE_RESET_SECS`] without one.
    pub last_hit_tick: u64,
}

/// Swing interval in ticks for whatever is in hand: tools and weapons use
/// their §4.1 use time, bare hands the canonized default.
fn use_ticks(held: Option<ItemId>) -> u64 {
    let secs = held
        .and_then(|i| {
            let d = i.data();
            d.tool.map(|t| t.use_secs).or(d.weapon.map(|w| w.use_secs))
        })
        .unwrap_or(BARE_HAND_USE_SECS);
    ((secs * TICK_RATE as f32).round() as u64).max(1)
}

/// Does a held tool satisfy a tile's tool requirement?
fn tool_matches(required: ToolKind, tool: Option<ToolStats>) -> bool {
    match required {
        ToolKind::Any => true,
        ToolKind::None => false,
        k => tool.is_some_and(|t| t.kind == k),
    }
}

/// Support rule for single-tile placement (Terraria-style): a wall behind
/// the cell, or a solid/platform tile cardinally adjacent.
fn has_support(world: &World, x: u32, y: u32) -> bool {
    if world.tile(x, y).wall != WallId::Air {
        return true;
    }
    let (xi, yi) = (x as i32, y as i32);
    [(xi - 1, yi), (xi + 1, yi), (xi, yi - 1), (xi, yi + 1)]
        .into_iter()
        .any(|(nx, ny)| {
            // Out-of-bounds reads count as solid for physics, but the world
            // border must not support placement.
            nx >= 0
                && ny >= 0
                && world.in_bounds(nx as u32, ny as u32)
                && (world.is_solid(nx, ny) || world.is_platform(nx, ny))
        })
}

/// Every bottom cell of a `w`-wide footprint at origin `(x, y)`, height `h`,
/// stands on solid ground or a platform (furniture floor rule).
fn floor_under(world: &World, x: u32, y: u32, w: u32, h: u32) -> bool {
    (0..w).all(|dx| {
        let (fx, fy) = ((x + dx) as i32, (y + h) as i32);
        world.is_solid(fx, fy) || world.is_platform(fx, fy)
    })
}

impl Sim {
    /// Validates reach + swing rate for player `id` aiming at `(x, y)`.
    /// Returns the held tool (if any) and consumes the swing cooldown.
    /// `None` means the swing was rejected (don't consume the cooldown for
    /// invalid targets — callers check the target *before* calling this).
    fn accept_swing(&mut self, id: u32, x: u32, y: u32) -> Option<Option<ToolStats>> {
        if !self.world.in_bounds(x, y) {
            return None;
        }
        let tick = self.tick;
        let p = self.players.get_mut(&id)?;
        if !tile_in_reach(p.center(), x, y) {
            return None;
        }
        let held = p
            .inventory
            .get(p.held_slot as usize)
            .copied()
            .flatten()
            .map(|s| s.item);
        // Server-enforced swing rate from the held item's use time, with 1
        // tick of tolerance for client/server tick-phase jitter.
        if let Some(last) = p.last_swing_tick {
            if tick.saturating_sub(last) + 1 < use_ticks(held) {
                return None;
            }
        }
        p.last_swing_tick = Some(tick);
        Some(held.and_then(|i| i.data().tool))
    }

    /// `HitTile`: one swing of the held tool at the foreground layer.
    pub(crate) fn hit_tile(&mut self, id: u32, x: u32, y: u32) {
        let tile = self.world.tile(x, y);
        let data = tile.id.data();
        if tile.id == TileId::Air || !data.breakable {
            // Ritual Altars are unbreakable (§2; the hammer-backlash damage
            // arrives with the combat systems — there is no player HP here
            // yet). Air is simply not a target.
            return;
        }
        let Some(tool) = self.accept_swing(id, x, y) else {
            return;
        };
        // §1.2 pass 9 forage: a mushroom plant on a grass cell harvests in
        // one hit from anything (the recipe-#47 ingredient), leaving the
        // grass itself undamaged.
        if tile.id == TileId::Grass && state::variant(tile.state) == state::GRASS_MUSHROOM {
            let mut t = tile;
            t.state &= !state::VARIANT_MASK;
            self.change_tile(x, y, t);
            self.spawn_item_drop(ItemId::Mushroom, 1, (x as f32 + 0.5, y as f32 + 0.5));
            return;
        }
        if !tool_matches(data.tool, tool) {
            return;
        }
        let power = tool.map(|t| t.power).unwrap_or(0);
        if power < data.min_power {
            return; // §2: below minimum power the tile takes zero damage
        }
        if data.one_hit {
            self.break_tile(x, y);
            return;
        }
        let dealt = power as f32 * data.hardness_mult;
        if dealt <= 0.0 {
            return;
        }
        if self.apply_damage((x, y, false), dealt) {
            self.break_tile(x, y);
        }
    }

    /// `HitWall`: hammers only (§2 walls). A wall sealed behind a solid
    /// foreground tile can't be hit — mirroring `place_wall`, and keeping
    /// the wall's drop from spawning inside the solid.
    pub(crate) fn hit_wall(&mut self, id: u32, x: u32, y: u32) {
        let tile = self.world.tile(x, y);
        if tile.wall == WallId::Air || tile.is_solid() {
            return;
        }
        let Some(tool) = self.accept_swing(id, x, y) else {
            return;
        };
        let Some(tool) = tool.filter(|t| t.kind == ToolKind::Hammer) else {
            return;
        };
        let dealt = tool.power as f32 * tile.wall.data().hardness_mult;
        if dealt <= 0.0 {
            return;
        }
        if self.apply_damage((x, y, true), dealt) {
            let mut t = self.world.tile(x, y);
            // §2: only player-placed walls drop their item.
            if t.state & state::WALL_PLACED != 0 {
                if let Some(item) = t.wall.data().drops {
                    self.spawn_item_drop(item, 1, (x as f32 + 0.5, y as f32 + 0.5));
                }
            }
            t.wall = WallId::Air;
            t.state &= !state::WALL_PLACED;
            self.change_tile(x, y, t);
        }
    }

    /// Adds swing damage to a cell (resetting first if the §2 5 s decay
    /// elapsed). Returns `true` when the cell's break-points are exhausted;
    /// otherwise broadcasts the crack overlay frame.
    fn apply_damage(&mut self, key: (u32, u32, bool), dealt: f32) -> bool {
        let reset_ticks = (TILE_DAMAGE_RESET_SECS * TICK_RATE as f32) as u64;
        let tick = self.tick;
        let entry = self.tile_damage.entry(key).or_insert(TileDamage {
            damage: 0.0,
            last_hit_tick: tick,
        });
        if tick.saturating_sub(entry.last_hit_tick) > reset_ticks {
            entry.damage = 0.0;
        }
        entry.damage += dealt;
        entry.last_hit_tick = tick;
        let damage = entry.damage;
        if damage >= TILE_BREAK_POINTS as f32 {
            self.tile_damage.remove(&key);
            return true;
        }
        let frac = ((damage / TILE_BREAK_POINTS as f32) * 255.0).min(255.0) as u8;
        self.broadcast_at(
            key.0,
            key.1,
            &ServerMessage::BlockCrack {
                x: key.0,
                y: key.1,
                damage_frac: frac,
            },
        );
        false
    }

    /// Breaks the foreground object at `(x, y)`: trees fell upward, pots
    /// roll loot, multi-tile furniture clears its whole footprint (refusing
    /// for non-empty chests), everything else drops its §2 item.
    pub(crate) fn break_tile(&mut self, x: u32, y: u32) {
        let tile = self.world.tile(x, y);
        match tile.id {
            TileId::Air => {}
            TileId::TreeTrunk => self.fell_tree(x, y),
            TileId::Pot => {
                self.clear_cell(x, y);
                let mult = loot::pot_coin_mult(y, self.world.height);
                let slot = loot::roll_pot(&mut self.loot_rng, mult);
                self.spawn_item_drop(slot.item, slot.count, (x as f32 + 0.5, y as f32 + 0.5));
            }
            TileId::Sapling => {
                self.saplings.remove(&(x, y));
                self.clear_cell(x, y);
            }
            id if id.data().size != (1, 1) => self.break_multitile(x, y),
            id => {
                // A forage plant still standing on breaking grass pops with
                // it (normally the forage swing in `hit_tile` harvests it
                // first, but other break paths reach here directly).
                if id == TileId::Grass && state::variant(tile.state) == state::GRASS_MUSHROOM {
                    self.spawn_item_drop(ItemId::Mushroom, 1, (x as f32 + 0.5, y as f32 + 0.5));
                }
                self.clear_cell(x, y);
                if let Some((item, n)) = id.data().drops {
                    self.spawn_item_drop(item, n as u16, (x as f32 + 0.5, y as f32 + 0.5));
                }
            }
        }
    }

    /// Clears one foreground cell (wall, liquid, and the WALL_PLACED bit
    /// survive) and lets anything resting on it react (sand above falls,
    /// trees/saplings above fall too).
    fn clear_cell(&mut self, x: u32, y: u32) {
        let mut t = self.world.tile(x, y);
        let was_solid = t.is_solid();
        t.id = TileId::Air;
        t.state &= state::WALL_PLACED;
        self.change_tile(x, y, t);
        if was_solid && y > 0 {
            let above = self.world.tile(x, y - 1);
            match above.id {
                TileId::TreeTrunk => self.fell_tree(x, y - 1),
                TileId::Sapling => self.break_tile(x, y - 1),
                _ => {}
            }
        }
    }

    /// §2 row 32: felling a segment fells everything above it, dropping 10
    /// wood + a 25% acorn per segment at each segment's position.
    fn fell_tree(&mut self, x: u32, y: u32) {
        let mut yy = y;
        loop {
            if self.world.tile(x, yy).id != TileId::TreeTrunk {
                break;
            }
            let mut t = self.world.tile(x, yy);
            t.id = TileId::Air;
            t.state &= state::WALL_PLACED;
            self.change_tile(x, yy, t);
            let center = (x as f32 + 0.5, yy as f32 + 0.5);
            self.spawn_item_drop(ItemId::Wood, WOOD_PER_TREE_SEGMENT as u16, center);
            if self.loot_rng.chance(ACORN_DROP_CHANCE) {
                self.spawn_item_drop(ItemId::Acorn, 1, center);
            }
            if yy == 0 {
                break;
            }
            yy -= 1;
        }
    }

    /// Breaks a multi-tile furniture object as a unit from any of its cells.
    fn break_multitile(&mut self, x: u32, y: u32) {
        let (ox, oy) = self.world.multitile_origin(x, y);
        let id = self.world.tile(ox, oy).id;
        let data = id.data();
        if id == TileId::Chest {
            let occupied = self
                .world
                .chests
                .get(&(ox, oy))
                .is_some_and(|slots| slots.iter().any(Option::is_some));
            if occupied {
                return; // §2: can't break a chest while non-empty
            }
            self.world.chests.remove(&(ox, oy));
        }
        let (w, h) = (data.size.0 as u32, data.size.1 as u32);
        for dy in 0..h {
            for dx in 0..w {
                if self.world.tile(ox + dx, oy + dy).id == id {
                    let mut t = self.world.tile(ox + dx, oy + dy);
                    t.id = TileId::Air;
                    t.state &= state::WALL_PLACED;
                    self.change_tile(ox + dx, oy + dy, t);
                }
            }
        }
        if let Some((item, n)) = data.drops {
            self.spawn_item_drop(
                item,
                n as u16,
                (ox as f32 + w as f32 / 2.0, oy as f32 + h as f32 / 2.0),
            );
        }
    }

    /// `PlaceTile`: validates reach, possession, §2 placement rules
    /// (emptiness, support, furniture floors, door frames, torch
    /// attachment), and that solids never overlap a player/entity hitbox,
    /// then consumes the item.
    pub(crate) fn place_tile(&mut self, id: u32, x: u32, y: u32, slot: u8) {
        if !self.world.in_bounds(x, y) || slot as usize >= inventory::HOTBAR {
            return;
        }
        let Some(p) = self.players.get(&id) else {
            return;
        };
        let center = p.center();
        let Some(stack) = p.inventory.get(slot as usize).copied().flatten() else {
            return;
        };
        let Some(Placement::Tile(tile_id)) = stack.item.data().places else {
            return;
        };
        let data = tile_id.data();
        let (w, h) = (data.size.0 as u32, data.size.1 as u32);

        // The whole footprint must be in reach (§8) — checking only the
        // origin would let a 4×2 bed extend cells ~4 tiles past it — and
        // empty (also checked atomically by place_multitile, but validating
        // first keeps refusals side-effect free).
        for dy in 0..h {
            for dx in 0..w {
                if !tile_in_reach(center, x + dx, y + dy) || !self.world.is_empty(x + dx, y + dy) {
                    return;
                }
            }
        }

        // Per-kind placement rules.
        let valid = match tile_id {
            // Acorns plant saplings on grass (§4.3).
            TileId::Sapling => {
                y + 1 < self.world.height && self.world.tile(x, y + 1).id == TileId::Grass
            }
            // Torches attach to an adjacent solid or a wall, never in water
            // (§2 tile 16).
            TileId::Torch => {
                let t = self.world.tile(x, y);
                t.liquid.kind() != Some(LiquidKind::Water) && has_support(&self.world, x, y)
            }
            // Doors need solid tiles directly above and below the 1×3 frame
            // (§2 tile 18).
            TileId::Door => {
                y > 0
                    && self.world.is_solid(x as i32, y as i32 - 1)
                    && self.world.is_solid(x as i32, (y + h) as i32)
            }
            // Other furniture stands on its floor.
            _ if data.furniture && data.size != (1, 1) => floor_under(&self.world, x, y, w, h),
            _ if data.furniture => {
                floor_under(&self.world, x, y, w, h) || has_support(&self.world, x, y)
            }
            // Plain blocks/platforms: the Terraria-style support rule.
            _ => has_support(&self.world, x, y),
        };
        if !valid {
            return;
        }

        // A placed solid must not overlap any player or entity hitbox.
        let solid = matches!(data.solidity, Solidity::Solid);
        if solid && self.footprint_overlaps_hitbox(x, y, w, h) {
            return;
        }

        // Apply.
        if data.size == (1, 1) {
            let mut t = self.world.tile(x, y);
            t.id = tile_id;
            t.state &= state::WALL_PLACED;
            if solid {
                // Solid blocks displace (destroy) the cell's liquid.
                t.liquid = Liquid::NONE;
            }
            self.change_tile(x, y, t);
        } else {
            if !self.world.place_multitile(x, y, tile_id) {
                return;
            }
            for dy in 0..h {
                for dx in 0..w {
                    let mut t = self.world.tile(x + dx, y + dy);
                    if solid {
                        // Solid footprints (Door) displace liquid like 1×1
                        // solids — water must not survive inside a closed
                        // door and drain out of it later.
                        t.liquid = Liquid::NONE;
                    }
                    self.change_tile(x + dx, y + dy, t);
                }
            }
            if tile_id == TileId::Chest {
                self.world
                    .chests
                    .insert((x, y), vec![None; ferraria_shared::world::CHEST_SLOTS]);
            }
        }
        if tile_id == TileId::Sapling {
            let grow_secs = self
                .loot_rng
                .gen_range_f32(SAPLING_GROW_MIN_SECS, SAPLING_GROW_MAX_SECS);
            self.saplings
                .insert((x, y), self.tick + (grow_secs * TICK_RATE as f32) as u64);
        }
        self.consume_item(id, slot);
    }

    /// `PlaceWall`: empty wall layer + Terraria-style support; the placed
    /// wall gets the WALL_PLACED bit so hammering it drops the item (§2).
    pub(crate) fn place_wall(&mut self, id: u32, x: u32, y: u32, slot: u8) {
        if !self.world.in_bounds(x, y) || slot as usize >= inventory::HOTBAR {
            return;
        }
        let Some(p) = self.players.get(&id) else {
            return;
        };
        if !tile_in_reach(p.center(), x, y) {
            return;
        }
        let Some(stack) = p.inventory.get(slot as usize).copied().flatten() else {
            return;
        };
        let Some(Placement::Wall(wall_id)) = stack.item.data().places else {
            return;
        };
        let t = self.world.tile(x, y);
        if t.wall != WallId::Air || t.is_solid() {
            return;
        }
        // Wall support: an adjacent wall or an adjacent solid/platform.
        let (xi, yi) = (x as i32, y as i32);
        let supported = [(xi - 1, yi), (xi + 1, yi), (xi, yi - 1), (xi, yi + 1)]
            .into_iter()
            .any(|(nx, ny)| {
                nx >= 0
                    && ny >= 0
                    && self.world.in_bounds(nx as u32, ny as u32)
                    && (self.world.tile(nx as u32, ny as u32).wall != WallId::Air
                        || self.world.is_solid(nx, ny)
                        || self.world.is_platform(nx, ny))
            });
        if !supported {
            return;
        }
        let mut t = t;
        t.wall = wall_id;
        t.state |= state::WALL_PLACED;
        self.change_tile(x, y, t);
        self.consume_item(id, slot);
    }

    /// `ToggleDoor` (§2 tile 18): opening swings the panel to the side away
    /// from the toggling player (falling back to the other side, refusing
    /// if both are blocked); closing refuses while any player or entity
    /// overlaps the doorway.
    pub(crate) fn toggle_door(&mut self, id: u32, x: u32, y: u32) {
        if !self.world.in_bounds(x, y) || self.world.tile(x, y).id != TileId::Door {
            return;
        }
        let tick = self.tick;
        let Some(p) = self.players.get(&id) else {
            return;
        };
        if !tile_in_reach(p.center(), x, y) {
            return;
        }
        // Anti-amplification: one toggle re-broadcasts the whole 1×3 column
        // to every chunk subscriber, so accepted toggles are spaced out.
        if p.last_door_toggle_tick
            .is_some_and(|t| tick.saturating_sub(t) < DOOR_TOGGLE_COOLDOWN_TICKS)
        {
            return;
        }
        let player_x = p.center().0;
        let (ox, oy) = self.world.multitile_origin(x, y);
        let open = self.world.tile(ox, oy).state & state::DOOR_OPEN != 0;
        let h = TileId::Door.data().size.1 as u32;
        if open {
            // Closing makes the column solid again: nobody may be standing
            // in it.
            if self.footprint_overlaps_hitbox(ox, oy, 1, h) {
                return;
            }
            self.consume_door_toggle(id);
            for dy in 0..h {
                let mut t = self.world.tile(ox, oy + dy);
                t.state &= !(state::DOOR_OPEN | state::DOOR_OPEN_LEFT);
                self.change_tile(ox, oy + dy, t);
            }
        } else {
            // The panel needs a clear side column; prefer the side away
            // from the player.
            let away_left = player_x > ox as f32 + 0.5;
            let side_clear = |sim: &Sim, left: bool| -> bool {
                let sx = if left {
                    (ox as i32) - 1
                } else {
                    (ox as i32) + 1
                };
                (0..h).all(|dy| {
                    sx >= 0
                        && sim.world.in_bounds(sx as u32, oy + dy)
                        && !sim.world.is_solid(sx, (oy + dy) as i32)
                })
            };
            let left = if side_clear(self, away_left) {
                away_left
            } else if side_clear(self, !away_left) {
                !away_left
            } else {
                return; // jammed: both sides blocked
            };
            self.consume_door_toggle(id);
            for dy in 0..h {
                let mut t = self.world.tile(ox, oy + dy);
                t.state |= state::DOOR_OPEN;
                if left {
                    t.state |= state::DOOR_OPEN_LEFT;
                } else {
                    t.state &= !state::DOOR_OPEN_LEFT;
                }
                self.change_tile(ox, oy + dy, t);
            }
        }
    }

    /// Marks an accepted `ToggleDoor` against the player's
    /// [`DOOR_TOGGLE_COOLDOWN_TICKS`] rate cap.
    fn consume_door_toggle(&mut self, id: u32) {
        let tick = self.tick;
        if let Some(p) = self.players.get_mut(&id) {
            p.last_door_toggle_tick = Some(tick);
        }
    }

    /// Drains the support-check queue (`Sim::queue_support_checks`):
    /// fixtures whose §2 support rule no longer holds pop, dropping their
    /// item — a torch loses its attachment (solid neighbor or wall), a door
    /// its lintel/sill, multi-tile furniture its floor. Pops re-queue their
    /// own neighborhoods, so chains (a torch attached to a popping door)
    /// resolve in the same pass; every pop removes an object, so the drain
    /// terminates. Non-empty chests are the §2 exception: they refuse to
    /// break and stay floating until emptied.
    pub(crate) fn revalidate_supports(&mut self) {
        while let Some((x, y)) = self.support_checks.pop() {
            let t = self.world.tile(x, y);
            match t.id {
                // §2 tile 16: needs a wall behind or an adjacent
                // solid/platform.
                TileId::Torch => {
                    if !has_support(&self.world, x, y) {
                        self.break_tile(x, y);
                    }
                }
                // §2 tile 18: needs solid tiles above and below the frame.
                TileId::Door => {
                    let (ox, oy) = self.world.multitile_origin(x, y);
                    let h = TileId::Door.data().size.1 as u32;
                    let framed = oy > 0
                        && self.world.is_solid(ox as i32, oy as i32 - 1)
                        && self.world.is_solid(ox as i32, (oy + h) as i32);
                    if !framed {
                        self.break_tile(x, y);
                    }
                }
                // Multi-tile furniture stands on its floor.
                id if id.data().furniture && id.data().size != (1, 1) => {
                    let (ox, oy) = self.world.multitile_origin(x, y);
                    let (w, h) = (id.data().size.0 as u32, id.data().size.1 as u32);
                    if !floor_under(&self.world, ox, oy, w, h) {
                        self.break_tile(x, y);
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether the tile-aligned rect `(x, y, w, h)` overlaps any player or
    /// entity AABB (shrunk by the collision skin so flush contacts — feet
    /// exactly on top of the placed tile — don't count).
    fn footprint_overlaps_hitbox(&self, x: u32, y: u32, w: u32, h: u32) -> bool {
        let (rx0, ry0) = (x as f32 + COLLISION_EPS, y as f32 + COLLISION_EPS);
        let (rx1, ry1) = (
            (x + w) as f32 - COLLISION_EPS,
            (y + h) as f32 - COLLISION_EPS,
        );
        let overlaps = |pos: (f32, f32), size: (f32, f32)| -> bool {
            pos.0 < rx1 && pos.0 + size.0 > rx0 && pos.1 < ry1 && pos.1 + size.1 > ry0
        };
        self.players
            .values()
            .any(|p| overlaps(p.pos, (PLAYER_WIDTH, PLAYER_HEIGHT)))
            || self
                .entities
                .map
                .values()
                .any(|e| overlaps(e.pos, e.size()))
    }

    /// Removes one item from `slot`, pushing the delta to the owner (and a
    /// held-item update to everyone else if their visible hand changed).
    fn consume_item(&mut self, id: u32, slot: u8) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let Some(Some(stack)) = p.inventory.get_mut(slot as usize) else {
            return;
        };
        stack.count = stack.count.saturating_sub(1);
        if stack.count == 0 {
            p.inventory[slot as usize] = None;
        }
        let new_stack = p.inventory[slot as usize];
        let held_changed = p.held_slot == slot && new_stack.is_none();
        self.send_to(
            id,
            &ServerMessage::SlotChanged {
                idx: slot,
                stack: new_stack,
            },
        );
        if held_changed {
            self.broadcast_held_item(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::entities::EntityKind;
    use super::super::game::Sim;
    use super::super::test_util::*;
    use super::*;
    use ferraria_shared::items::InvSlot;
    use ferraria_shared::protocol::ClientMessage;
    use tokio::sync::mpsc;

    const FLOOR: u32 = 30;

    /// Sim + one joined player standing on the floor near the middle.
    fn setup() -> (Sim, u32, u64, mpsc::Receiver<super::super::game::Frame>) {
        let mut sim = flat_sim(100, 60, FLOOR);
        let (id, epoch, mut rx) = join(&mut sim, "miner");
        drain(&mut rx);
        place_player(&mut sim, id, 50.0, FLOOR as f32);
        (sim, id, epoch, rx)
    }

    fn set(sim: &mut Sim, x: u32, y: u32, id: TileId) {
        let mut t = sim.world().tile(x, y);
        t.id = id;
        t.state = 0;
        sim.change_tile(x, y, t);
    }

    /// Counts how many of `item` are sitting in drop entities.
    fn dropped(sim: &Sim, item: ItemId) -> u32 {
        sim.entities
            .map
            .values()
            .map(|e| match e.kind {
                EntityKind::ItemDrop { item: i, count } if i == item => count as u32,
                _ => 0,
            })
            .sum()
    }

    /// Swings until `(x, y)` breaks; returns how many swings it took.
    fn swings_to_break(
        sim: &mut Sim,
        id: u32,
        epoch: u64,
        x: u32,
        y: u32,
        max: u32,
    ) -> Option<u32> {
        for n in 1..=max {
            swing_tile(sim, id, epoch, x, y);
            if sim.world().tile(x, y).id == TileId::Air {
                return Some(n);
            }
        }
        None
    }

    #[test]
    fn mining_math_per_tier_and_hellstone_gate() {
        let (mut sim, id, epoch, _rx) = setup();
        // §2/§4.1: swings = ceil(100 / (power × mult)).
        let cases: [(ItemId, TileId, u32); 5] = [
            (ItemId::WoodPickaxe, TileId::Dirt, 2),        // 25×2 = 50
            (ItemId::WoodPickaxe, TileId::Stone, 4),       // 25×1 = 25
            (ItemId::SilverPickaxe, TileId::SilverOre, 3), // 45×0.75 = 33.75
            (ItemId::GoldPickaxe, TileId::Hellstone, 4),   // 55×0.5 = 27.5
            (ItemId::EmberPickaxe, TileId::Obsidian, 2),   // 100×0.5 = 50
        ];
        // Two tiles of margin past the pickup radius: drops arc toward the
        // player but must never be auto-collected mid-test.
        let (x, y) = (54, FLOOR - 1);
        for (tool, tile, expect) in cases {
            set(&mut sim, x, y, tile);
            give(&mut sim, id, 0, tool, 1);
            let got = swings_to_break(&mut sim, id, epoch, x, y, 10);
            assert_eq!(got, Some(expect), "{tool:?} vs {tile:?}");
        }

        // Hellstone gate: a 45-power pick deals zero damage forever.
        set(&mut sim, x, y, TileId::Hellstone);
        give(&mut sim, id, 0, ItemId::SilverPickaxe, 1);
        assert_eq!(swings_to_break(&mut sim, id, epoch, x, y, 10), None);
        assert!(sim.tile_damage.is_empty(), "no damage accumulated");

        // Wrong tool class: axes don't mine stone, picks don't chop trees.
        set(&mut sim, x, y, TileId::Stone);
        give(&mut sim, id, 0, ItemId::EmberAxe, 1);
        assert_eq!(swings_to_break(&mut sim, id, epoch, x, y, 5), None);
        set(&mut sim, x, y, TileId::TreeTrunk);
        give(&mut sim, id, 0, ItemId::EmberPickaxe, 1);
        assert_eq!(swings_to_break(&mut sim, id, epoch, x, y, 5), None);
    }

    #[test]
    fn broken_tiles_drop_their_item_and_announce_cracks() {
        let (mut sim, id, epoch, mut rx) = setup();
        let (x, y) = (55, FLOOR - 1);
        set(&mut sim, x, y, TileId::Stone);
        give(&mut sim, id, 0, ItemId::WoodPickaxe, 1);
        drain(&mut rx);
        let mut fracs = Vec::new();
        for _ in 0..4 {
            swing_tile(&mut sim, id, epoch, x, y);
            for m in drain(&mut rx) {
                if let ServerMessage::BlockCrack {
                    x: cx,
                    y: cy,
                    damage_frac,
                } = m
                {
                    assert_eq!((cx, cy), (x, y));
                    fracs.push(damage_frac);
                }
            }
        }
        // 3 partial hits crack (25/50/75 points), the 4th breaks.
        assert_eq!(fracs.len(), 3, "cracks: {fracs:?}");
        assert!(
            fracs.windows(2).all(|w| w[0] < w[1]),
            "increasing {fracs:?}"
        );
        assert_eq!(sim.world().tile(x, y).id, TileId::Air);
        assert_eq!(dropped(&sim, ItemId::Stone), 1);
    }

    #[test]
    fn swing_rate_is_enforced_with_one_tick_jitter() {
        let (mut sim, id, epoch, _rx) = setup();
        let (x, y) = (54, FLOOR - 1);
        // Wood pickaxe: 0.30 s = 18 ticks per swing; dirt dies in 2 swings.
        give(&mut sim, id, 0, ItemId::WoodPickaxe, 1);

        // Back-to-back swings: the second is ignored.
        set(&mut sim, x, y, TileId::Dirt);
        msg(&mut sim, id, epoch, ClientMessage::HitTile { x, y });
        advance(&mut sim, 1);
        msg(&mut sim, id, epoch, ClientMessage::HitTile { x, y });
        assert_eq!(sim.world().tile(x, y).id, TileId::Dirt, "spam rejected");

        // 16 ticks after the first accepted swing: still too soon.
        advance(&mut sim, 15); // 16 ticks since the accepted swing in total
        msg(&mut sim, id, epoch, ClientMessage::HitTile { x, y });
        assert_eq!(sim.world().tile(x, y).id, TileId::Dirt);

        // 17 ticks (= use time − 1, the tolerated jitter): accepted.
        advance(&mut sim, 1);
        msg(&mut sim, id, epoch, ClientMessage::HitTile { x, y });
        assert_eq!(sim.world().tile(x, y).id, TileId::Air, "2nd swing landed");
    }

    #[test]
    fn tile_damage_resets_after_five_seconds() {
        let (mut sim, id, epoch, _rx) = setup();
        let (x, y) = (55, FLOOR - 1);
        set(&mut sim, x, y, TileId::Stone);
        give(&mut sim, id, 0, ItemId::WoodPickaxe, 1);
        swing_tile(&mut sim, id, epoch, x, y); // 25 points
        advance(&mut sim, 310); // > 5 s without hits: decays
        for _ in 0..3 {
            swing_tile(&mut sim, id, epoch, x, y);
        }
        // 3 post-decay swings = 75 points: without the reset it would have
        // broken on the 4th hit overall.
        assert_eq!(sim.world().tile(x, y).id, TileId::Stone);
        swing_tile(&mut sim, id, epoch, x, y);
        assert_eq!(sim.world().tile(x, y).id, TileId::Air);
    }

    #[test]
    fn reach_limits_swings_and_placement() {
        let (mut sim, id, epoch, _rx) = setup();
        give(&mut sim, id, 0, ItemId::EmberPickaxe, 1);
        // Floor tile 20 columns away: out of the 6-tile reach.
        let (x, y) = (70, FLOOR);
        swing_tile(&mut sim, id, epoch, x, y);
        assert_eq!(sim.world().tile(x, y).id, TileId::Stone);
        give(&mut sim, id, 0, ItemId::Dirt, 10);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x,
                y: FLOOR - 1,
                hotbar_slot: 0,
            },
        );
        assert_eq!(sim.world().tile(x, FLOOR - 1).id, TileId::Air);
        assert_eq!(
            sim.players[&id].inventory[0],
            Some(InvSlot::new(ItemId::Dirt, 10)),
            "out-of-reach placement consumes nothing"
        );
    }

    #[test]
    fn tree_felling_drops_per_segment_from_the_cut_upward() {
        let (mut sim, id, epoch, _rx) = setup();
        let x = 54; // wood drops stay out of auto-pickup range
                    // 5 trunk segments standing on the floor: rows 25..=29.
        for i in 0..5u32 {
            let y = FLOOR - 1 - i;
            let mut t = sim.world().tile(x, y);
            t.id = TileId::TreeTrunk;
            t.state = if i == 4 {
                state::TREE_SEGMENT_TOP
            } else {
                state::TREE_SEGMENT_TRUNK
            };
            sim.change_tile(x, y, t);
        }
        give(&mut sim, id, 0, ItemId::EmberAxe, 1); // 100 power: 1 swing/segment
                                                    // Cut the middle segment (row 27): it and the 2 above fall.
        swing_tile(&mut sim, id, epoch, x, FLOOR - 3);
        for y in [FLOOR - 3, FLOOR - 4, FLOOR - 5] {
            assert_eq!(sim.world().tile(x, y).id, TileId::Air, "felled row {y}");
        }
        for y in [FLOOR - 1, FLOOR - 2] {
            assert_eq!(sim.world().tile(x, y).id, TileId::TreeTrunk, "kept {y}");
        }
        assert_eq!(
            dropped(&sim, ItemId::Wood),
            3 * WOOD_PER_TREE_SEGMENT as u32
        );
        assert!(dropped(&sim, ItemId::Acorn) <= 3, "≤1 acorn per segment");

        // Fell the stump: 2 more segments of wood.
        swing_tile(&mut sim, id, epoch, x, FLOOR - 1);
        assert_eq!(
            dropped(&sim, ItemId::Wood),
            5 * WOOD_PER_TREE_SEGMENT as u32
        );
    }

    #[test]
    fn pots_break_in_one_hit_from_anything_and_roll_loot() {
        let (mut sim, id, epoch, _rx) = setup();
        let (x, y) = (54, FLOOR - 1);
        set(&mut sim, x, y, TileId::Pot);
        give_nothing(&mut sim, id, 0); // bare hands qualify (ToolKind::Any)
        swing_tile(&mut sim, id, epoch, x, y);
        assert_eq!(sim.world().tile(x, y).id, TileId::Air);
        let drops: Vec<(ItemId, u16)> = sim
            .entities
            .map
            .values()
            .filter_map(|e| match e.kind {
                EntityKind::ItemDrop { item, count } => Some((item, count)),
                _ => None,
            })
            .collect();
        assert_eq!(drops.len(), 1);
        let (item, count) = drops[0];
        // §2.3 coin rolls scale with depth; this floor row sits in the
        // scaled world's cavern band (loot::pot_coin_mult).
        let mult = loot::pot_coin_mult(y, sim.world().height);
        match item {
            ItemId::SilverCoin => assert!((mult..=10 * mult).contains(&count)),
            ItemId::Torch => assert!((3..=8).contains(&count)),
            ItemId::LesserHealingPotion => assert_eq!(count, 1),
            ItemId::WoodenArrow => assert!((10..=20).contains(&count)),
            ItemId::Gel => assert!((1..=4).contains(&count)),
            other => panic!("{other:?} not in the §2.3 pot table"),
        }
    }

    #[test]
    fn chest_with_items_refuses_to_break() {
        let (mut sim, id, epoch, _rx) = setup();
        let (x, y) = (53, FLOOR - 2);
        assert!(sim.world.place_multitile(x, y, TileId::Chest));
        let mut slots = vec![None; ferraria_shared::world::CHEST_SLOTS];
        slots[7] = Some(InvSlot::new(ItemId::Gel, 3));
        sim.world.chests.insert((x, y), slots);

        give(&mut sim, id, 0, ItemId::WoodPickaxe, 1);
        // Hit a NON-origin cell: multi-tile resolution must still find it.
        swing_tile(&mut sim, id, epoch, x + 1, y + 1);
        assert_eq!(sim.world().tile(x, y).id, TileId::Chest, "refused");
        assert!(sim.world.chests.contains_key(&(x, y)));

        // Emptied: breaks as a unit and drops the chest item.
        sim.world
            .chests
            .insert((x, y), vec![None; ferraria_shared::world::CHEST_SLOTS]);
        swing_tile(&mut sim, id, epoch, x + 1, y);
        for dy in 0..2 {
            for dx in 0..2 {
                assert_eq!(sim.world().tile(x + dx, y + dy).id, TileId::Air);
            }
        }
        assert!(!sim.world.chests.contains_key(&(x, y)));
        assert_eq!(dropped(&sim, ItemId::Chest), 1);
    }

    #[test]
    fn placement_validation_suite() {
        let (mut sim, id, epoch, mut rx) = setup();
        give(&mut sim, id, 0, ItemId::Dirt, 50);
        let place = |sim: &mut Sim, x: u32, y: u32| {
            msg(
                sim,
                id,
                epoch,
                ClientMessage::PlaceTile {
                    x,
                    y,
                    hotbar_slot: 0,
                },
            );
        };

        // No support: floating in mid-air with no wall/neighbor.
        place(&mut sim, 53, FLOOR - 5);
        assert_eq!(sim.world().tile(53, FLOOR - 5).id, TileId::Air);

        // Inside-wall: the target cell is already occupied.
        place(&mut sim, 53, FLOOR);
        assert_eq!(sim.world().tile(53, FLOOR).id, TileId::Stone);

        // Overlapping the player: solid placement refused.
        let feet = (50, FLOOR - 1); // player center column, feet row
        place(&mut sim, feet.0, feet.1);
        assert_eq!(sim.world().tile(feet.0, feet.1).id, TileId::Air);

        // Valid: on top of the floor, two tiles to the side.
        assert_eq!(sim.players[&id].inventory[0].map(|s| s.count), Some(50));
        drain(&mut rx);
        place(&mut sim, 53, FLOOR - 1);
        assert_eq!(sim.world().tile(53, FLOOR - 1).id, TileId::Dirt);
        assert_eq!(sim.players[&id].inventory[0].map(|s| s.count), Some(49));
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::TileChanged { x: 53, tile, .. } if tile.id == TileId::Dirt)));
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::SlotChanged { idx: 0, stack: Some(s) } if s.count == 49)));

        // Door without a frame: needs solid above AND below.
        give(&mut sim, id, 0, ItemId::Door, 5);
        place(&mut sim, 47, FLOOR - 3); // top of a 1×3 ending on the floor
        assert_eq!(
            sim.world().tile(47, FLOOR - 3).id,
            TileId::Air,
            "no lintel above"
        );
        // Build the lintel and retry.
        set(&mut sim, 47, FLOOR - 4, TileId::Stone);
        place(&mut sim, 47, FLOOR - 3);
        for dy in 0..3 {
            let t = sim.world().tile(47, FLOOR - 3 + dy);
            assert_eq!(t.id, TileId::Door);
            assert_eq!(state::part_y(t.state) as u32, dy);
        }

        // Torch: not in water, needs a wall or adjacent solid.
        give(&mut sim, id, 0, ItemId::Torch, 5);
        place(&mut sim, 55, FLOOR - 5); // mid-air: no attach point
        assert_eq!(sim.world().tile(55, FLOOR - 5).id, TileId::Air);
        let mut wet = sim.world().tile(55, FLOOR - 1);
        wet.liquid = Liquid::new(LiquidKind::Water, 4);
        sim.change_tile(55, FLOOR - 1, wet);
        place(&mut sim, 55, FLOOR - 1); // on the floor but submerged
        assert_eq!(sim.world().tile(55, FLOOR - 1).id, TileId::Air);
        place(&mut sim, 54, FLOOR - 1); // dry, floor below: ok
        assert_eq!(sim.world().tile(54, FLOOR - 1).id, TileId::Torch);

        // Furniture floor rule: a workbench can't hang in the air.
        give(&mut sim, id, 0, ItemId::Workbench, 2);
        place(&mut sim, 49, FLOOR - 4);
        assert_eq!(sim.world().tile(49, FLOOR - 4).id, TileId::Air);
        place(&mut sim, 51, FLOOR - 1);
        assert_eq!(sim.world().tile(51, FLOOR - 1).id, TileId::Workbench);
        assert_eq!(sim.world().tile(52, FLOOR - 1).id, TileId::Workbench);
    }

    #[test]
    fn placed_walls_drop_natural_walls_do_not() {
        let (mut sim, id, epoch, _rx) = setup();
        give(&mut sim, id, 0, ItemId::WoodWall, 5);
        let (x, y) = (54, FLOOR - 1); // wall against the floor: supported
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceWall {
                x,
                y,
                hotbar_slot: 0,
            },
        );
        let t = sim.world().tile(x, y);
        assert_eq!(t.wall, WallId::Wood);
        assert_ne!(t.state & state::WALL_PLACED, 0);
        assert_eq!(sim.players[&id].inventory[0].map(|s| s.count), Some(4));

        // Floating wall placement is refused.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceWall {
                x: 55,
                y: FLOOR - 6,
                hotbar_slot: 0,
            },
        );
        assert_eq!(sim.world().tile(55, FLOOR - 6).wall, WallId::Air);

        // Hammering the placed wall drops it (one swing: 55 × 2.0 = 110).
        give(&mut sim, id, 0, ItemId::IronHammer, 1);
        msg(&mut sim, id, epoch, ClientMessage::HitWall { x, y });
        advance(&mut sim, 40);
        assert_eq!(sim.world().tile(x, y).wall, WallId::Air);
        assert_eq!(dropped(&sim, ItemId::WoodWall), 1);

        // A natural wall (no WALL_PLACED bit) breaks but drops nothing.
        let mut nat = sim.world().tile(x, y);
        nat.wall = WallId::Dirt;
        nat.state &= !state::WALL_PLACED;
        sim.change_tile(x, y, nat);
        msg(&mut sim, id, epoch, ClientMessage::HitWall { x, y });
        advance(&mut sim, 40);
        assert_eq!(sim.world().tile(x, y).wall, WallId::Air);
        assert_eq!(dropped(&sim, ItemId::DirtWall), 0);
    }

    #[test]
    fn doors_toggle_away_from_the_player_and_refuse_blocked_closes() {
        let (mut sim, id, epoch, _rx) = setup();
        // Door frame at column 53, rows 27..=29, lintel at 26.
        let x = 53;
        set(&mut sim, x, FLOOR - 4, TileId::Stone);
        give(&mut sim, id, 0, ItemId::Door, 1);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x,
                y: FLOOR - 3,
                hotbar_slot: 0,
            },
        );
        assert_eq!(sim.world().tile(x, FLOOR - 1).id, TileId::Door);
        assert!(sim.world().tile(x, FLOOR - 1).is_solid(), "closed = solid");

        // Player stands left of the door: panel opens right (away).
        place_player(&mut sim, id, 50.0, FLOOR as f32);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::ToggleDoor { x, y: FLOOR - 2 },
        );
        for dy in 1..=3 {
            let t = sim.world().tile(x, FLOOR - dy);
            assert_ne!(t.state & state::DOOR_OPEN, 0, "open row {dy}");
            assert_eq!(t.state & state::DOOR_OPEN_LEFT, 0, "panel right");
            assert!(!t.is_solid(), "open = passable");
        }

        // A player standing in the doorway blocks closing.
        place_player(&mut sim, id, x as f32 + 0.5, FLOOR as f32);
        advance(&mut sim, DOOR_TOGGLE_COOLDOWN_TICKS as u32); // clear the rate cap
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::ToggleDoor { x, y: FLOOR - 1 },
        );
        assert_ne!(
            sim.world().tile(x, FLOOR - 1).state & state::DOOR_OPEN,
            0,
            "refused to close on an occupant"
        );

        // Step aside: closes (and the panel-side bit clears).
        place_player(&mut sim, id, 56.0, FLOOR as f32);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::ToggleDoor { x, y: FLOOR - 1 },
        );
        let t = sim.world().tile(x, FLOOR - 1);
        assert_eq!(t.state & (state::DOOR_OPEN | state::DOOR_OPEN_LEFT), 0);
        assert!(t.is_solid());

        // Player now right of the door: panel opens left.
        advance(&mut sim, DOOR_TOGGLE_COOLDOWN_TICKS as u32);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::ToggleDoor { x, y: FLOOR - 1 },
        );
        assert_ne!(
            sim.world().tile(x, FLOOR - 1).state & state::DOOR_OPEN_LEFT,
            0
        );
    }

    #[test]
    fn mushroom_forage_yields_exactly_one_mushroom() {
        let (mut sim, id, epoch, _rx) = setup();
        let (x, y) = (54, FLOOR - 1);
        // A grass cell carrying the worldgen forage variant (§1.2 pass 9).
        let mut t = sim.world().tile(x, y);
        t.id = TileId::Grass;
        t.state = state::GRASS_MUSHROOM;
        sim.change_tile(x, y, t);

        // Bare hands forage it in one swing: +1 Mushroom, grass untouched.
        give_nothing(&mut sim, id, 0);
        swing_tile(&mut sim, id, epoch, x, y);
        assert_eq!(sim.world().tile(x, y).id, TileId::Grass, "grass survives");
        assert_eq!(
            state::variant(sim.world().tile(x, y).state),
            0,
            "variant cleared"
        );
        assert_eq!(dropped(&sim, ItemId::Mushroom), 1);

        // Re-foraging the bare cell yields nothing (no dupes).
        swing_tile(&mut sim, id, epoch, x, y);
        assert_eq!(dropped(&sim, ItemId::Mushroom), 1);

        // Other break paths on mushroom-bearing grass free the mushroom too.
        let mut t = sim.world().tile(x, y);
        t.state = state::GRASS_MUSHROOM;
        sim.change_tile(x, y, t);
        sim.break_tile(x, y);
        assert_eq!(dropped(&sim, ItemId::Mushroom), 2);
        assert_eq!(dropped(&sim, ItemId::Dirt), 1, "grass still drops dirt");
    }

    #[test]
    fn fixtures_pop_when_their_support_is_removed() {
        let (mut sim, id, epoch, _rx) = setup();

        // Torch standing on a dirt block: mining the block pops the torch
        // (end-to-end through the HitTile intent).
        set(&mut sim, 54, FLOOR - 1, TileId::Dirt);
        set(&mut sim, 54, FLOOR - 2, TileId::Torch);
        give(&mut sim, id, 0, ItemId::EmberPickaxe, 1);
        swing_tile(&mut sim, id, epoch, 54, FLOOR - 1);
        assert_eq!(
            sim.world().tile(54, FLOOR - 2).id,
            TileId::Air,
            "torch popped"
        );
        assert_eq!(dropped(&sim, ItemId::Torch), 1);

        // Door: removing the lintel pops the whole door, dropping exactly
        // one (the re-validation pass must not double-break the multitile).
        let x = 47;
        set(&mut sim, x, FLOOR - 4, TileId::Stone);
        give(&mut sim, id, 0, ItemId::Door, 1);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x,
                y: FLOOR - 3,
                hotbar_slot: 0,
            },
        );
        assert_eq!(sim.world().tile(x, FLOOR - 1).id, TileId::Door);
        set(&mut sim, x, FLOOR - 4, TileId::Air);
        sim.revalidate_supports();
        for dy in 1..=3 {
            assert_eq!(
                sim.world().tile(x, FLOOR - dy).id,
                TileId::Air,
                "door fell apart (row -{dy})"
            );
        }
        assert_eq!(dropped(&sim, ItemId::Door), 1, "exactly one door dropped");

        // Workbench: knocking out part of its floor pops it.
        assert!(sim.world.place_multitile(44, FLOOR - 1, TileId::Workbench));
        set(&mut sim, 44, FLOOR, TileId::Air);
        sim.revalidate_supports();
        assert_eq!(sim.world().tile(44, FLOOR - 1).id, TileId::Air);
        assert_eq!(sim.world().tile(45, FLOOR - 1).id, TileId::Air);
        assert_eq!(dropped(&sim, ItemId::Workbench), 1);

        // §2 exception: a non-empty chest refuses to break and floats.
        assert!(sim.world.place_multitile(41, FLOOR - 2, TileId::Chest));
        let mut slots = vec![None; ferraria_shared::world::CHEST_SLOTS];
        slots[0] = Some(InvSlot::new(ItemId::Gel, 1));
        sim.world.chests.insert((41, FLOOR - 2), slots);
        set(&mut sim, 41, FLOOR, TileId::Air);
        set(&mut sim, 42, FLOOR, TileId::Air);
        sim.revalidate_supports();
        assert_eq!(
            sim.world().tile(41, FLOOR - 2).id,
            TileId::Chest,
            "non-empty chest survives unsupported"
        );
    }

    #[test]
    fn doors_placed_in_water_displace_it() {
        let (mut sim, id, epoch, _rx) = setup();
        let x = 53;
        set(&mut sim, x, FLOOR - 4, TileId::Stone); // lintel
        for dy in 1..=3 {
            let mut t = sim.world().tile(x, FLOOR - dy);
            t.liquid = Liquid::new(LiquidKind::Water, 8);
            sim.change_tile(x, FLOOR - dy, t);
        }
        give(&mut sim, id, 0, ItemId::Door, 1);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x,
                y: FLOOR - 3,
                hotbar_slot: 0,
            },
        );
        for dy in 1..=3 {
            let t = sim.world().tile(x, FLOOR - dy);
            assert_eq!(t.id, TileId::Door);
            assert!(
                t.liquid.is_none(),
                "solid door cell must hold no water (row -{dy})"
            );
        }
    }

    #[test]
    fn multitile_placement_requires_full_footprint_reach() {
        let (mut sim, id, epoch, _rx) = setup();
        give(&mut sim, id, 0, ItemId::Bed, 2);
        // Origin in reach, but the 4×2 bed's far cells stretch ~3 tiles
        // past the §8 limit: refused, nothing consumed.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x: 55,
                y: FLOOR - 2,
                hotbar_slot: 0,
            },
        );
        assert_eq!(sim.world().tile(55, FLOOR - 2).id, TileId::Air);
        assert_eq!(sim.players[&id].inventory[0].map(|s| s.count), Some(2));
        // Whole footprint in reach: placed.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x: 52,
                y: FLOOR - 2,
                hotbar_slot: 0,
            },
        );
        assert_eq!(sim.world().tile(52, FLOOR - 2).id, TileId::Bed);
        assert_eq!(sim.world().tile(55, FLOOR - 1).id, TileId::Bed);
        assert_eq!(sim.players[&id].inventory[0].map(|s| s.count), Some(1));
    }

    #[test]
    fn walls_behind_solid_blocks_cannot_be_hammered() {
        let (mut sim, id, epoch, _rx) = setup();
        // A player-placed wall sealed behind a stone block.
        let (x, y) = (54, FLOOR - 1);
        let mut t = sim.world().tile(x, y);
        t.id = TileId::Stone;
        t.wall = WallId::Wood;
        t.state = state::WALL_PLACED;
        sim.change_tile(x, y, t);
        give(&mut sim, id, 0, ItemId::IronHammer, 1);
        msg(&mut sim, id, epoch, ClientMessage::HitWall { x, y });
        advance(&mut sim, 40);
        assert_eq!(
            sim.world().tile(x, y).wall,
            WallId::Wood,
            "sealed wall takes no hits (its drop would embed in the solid)"
        );

        // Mine the block; the wall hammers normally again.
        give(&mut sim, id, 0, ItemId::EmberPickaxe, 1);
        swing_tile(&mut sim, id, epoch, x, y);
        assert_eq!(sim.world().tile(x, y).id, TileId::Air);
        give(&mut sim, id, 0, ItemId::IronHammer, 1);
        msg(&mut sim, id, epoch, ClientMessage::HitWall { x, y });
        assert_eq!(sim.world().tile(x, y).wall, WallId::Air);
        assert_eq!(dropped(&sim, ItemId::WoodWall), 1);
    }

    #[test]
    fn door_toggles_are_rate_limited() {
        let (mut sim, id, epoch, _rx) = setup();
        let x = 53;
        set(&mut sim, x, FLOOR - 4, TileId::Stone);
        give(&mut sim, id, 0, ItemId::Door, 1);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x,
                y: FLOOR - 3,
                hotbar_slot: 0,
            },
        );
        let is_open = |sim: &Sim| sim.world().tile(x, FLOOR - 1).state & state::DOOR_OPEN != 0;

        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::ToggleDoor { x, y: FLOOR - 2 },
        );
        assert!(is_open(&sim), "first toggle accepted");
        // Same-tick spam: dropped (one toggle = 3 broadcast tile deltas).
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::ToggleDoor { x, y: FLOOR - 2 },
        );
        assert!(is_open(&sim), "spam toggle dropped");
        // One tick shy of the cooldown: still dropped.
        advance(&mut sim, DOOR_TOGGLE_COOLDOWN_TICKS as u32 - 1);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::ToggleDoor { x, y: FLOOR - 2 },
        );
        assert!(is_open(&sim), "cooldown still active");
        // Cooldown elapsed: the toggle lands.
        advance(&mut sim, 1);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::ToggleDoor { x, y: FLOOR - 2 },
        );
        assert!(!is_open(&sim), "post-cooldown toggle accepted");
    }

    #[test]
    fn acorns_plant_saplings_on_grass_only() {
        let (mut sim, id, epoch, _rx) = setup();
        give(&mut sim, id, 0, ItemId::Acorn, 2);
        let (x, y) = (52, FLOOR - 1);
        // On stone: refused.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x,
                y,
                hotbar_slot: 0,
            },
        );
        assert_eq!(sim.world().tile(x, y).id, TileId::Air);
        // On grass: planted and scheduled to grow.
        set(&mut sim, x, FLOOR, TileId::Grass);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlaceTile {
                x,
                y,
                hotbar_slot: 0,
            },
        );
        assert_eq!(sim.world().tile(x, y).id, TileId::Sapling);
        let due = sim.saplings.get(&(x, y)).copied().expect("scheduled");
        let min = (SAPLING_GROW_MIN_SECS * TICK_RATE as f32) as u64;
        let max = (SAPLING_GROW_MAX_SECS * TICK_RATE as f32) as u64;
        assert!(
            (min..=max).contains(&due.saturating_sub(sim.tick)),
            "due {due}"
        );
    }
}
