//! Types shared between the Ferraria server and client: tile/item/world data
//! tables and model, deterministic RNG, player physics, crafting, and the
//! WebSocket wire protocol.
//!
//! No tokio, no macroquad, no I/O — everything here compiles for both native
//! and wasm32-unknown-unknown and is pure/deterministic (ARCHITECTURE.md).
//! Gameplay numbers live in the data tables and constants, sourced from
//! DESIGN.md; don't inline them elsewhere.

pub mod crafting;
pub mod enemies;
pub mod inventory_ops;
pub mod items;
pub mod loadout;
mod macros;
pub mod physics;
pub mod protocol;
pub mod rng;
pub mod tiles;
pub mod world;

// Most-used physics constants, re-exported at the root (§0).
pub use physics::{GRAVITY, TERMINAL_VELOCITY};

/// Protocol version; bumped on every breaking wire change. The server
/// rejects clients with a mismatching version at handshake.
///
/// v2: the feature merge train (world-interact, lighting/day-night,
/// inventory/crafting) appended `ClientMessage`/`ServerMessage` variants in
/// a different order than any single pre-merge build understood.
pub const PROTOCOL_VERSION: u32 = 2;

/// World tiles are 16x16 px on screen; physics positions are in tile units.
pub const TILE_SIZE: f32 = 16.0;

/// Simulation rate, ticks per second.
pub const TICK_RATE: u32 = 60;

/// Seconds per tick. Velocities are tiles/second; multiply by `DT` per tick.
pub const DT: f32 = 1.0 / TICK_RATE as f32;

/// Players can mine/place/interact within this many tiles of their center.
pub const REACH: f32 = 6.0;

/// Whether the tile cell `(x, y)` is within [`REACH`] of a player whose
/// center is `center` (§8). Measured center-to-cell-center, Euclidean; the
/// client uses the same test to color the cursor highlight, so both sides
/// must agree.
pub fn tile_in_reach(center: (f32, f32), x: u32, y: u32) -> bool {
    let dx = center.0 - (x as f32 + 0.5);
    let dy = center.1 - (y as f32 + 0.5);
    dx * dx + dy * dy <= REACH * REACH
}

// ---- Item-drop entities (§2 drops, §11 "dropped items") ---------------------
// DESIGN fixes the behaviors (world-shared, first pickup wins, destroyed in
// lava, death piles persist 10 min); the v1 magnitudes are canonized here.

/// A fresh drop can't be picked up for this long (so mining doesn't vacuum
/// the block straight into the inventory before it's visible).
pub const ITEM_PICKUP_ARM_SECS: f32 = 0.5;
/// Players auto-collect armed drops within this distance of their hitbox.
pub const ITEM_PICKUP_RADIUS: f32 = 1.5;
/// Same-item drops within this distance merge into one stack.
pub const ITEM_MERGE_RADIUS: f32 = 1.0;
/// Drops despawn after 10 minutes (matches the §8 death-pile persistence).
pub const ITEM_DESPAWN_SECS: f32 = 600.0;

// ---- Netcode (ARCHITECTURE.md "Wire protocol" / "Authority model") ---------

/// Hard cap on concurrently connected players; the server rejects further
/// handshakes.
pub const MAX_PLAYERS: usize = 16;

/// Sanity clamp on client-reported velocity components (tiles/s). Generous —
/// well above terminal velocity — so legit play never trips it.
pub const MAX_PLAYER_SPEED: f32 = 50.0;

/// A client-reported position that jumps more than this many tiles per tick
/// since its last accepted state is rejected and snapped back.
pub const MAX_TELEPORT_PER_TICK: f32 = 30.0;

/// Cap on the banked movement allowance behind the teleport clamp. Accepted
/// displacement draws from a per-player budget that refills at
/// [`MAX_TELEPORT_PER_TICK`] per elapsed sim tick but never exceeds this, so
/// stacking many `PlayerState` messages into one tick (or going quiet for a
/// long time first) can never multiply the clamp into a map-wide teleport.
/// 10 ticks of allowance covers any legitimate burst: real movement tops out
/// at terminal velocity (37.5 t/s ≈ 0.6 tiles/tick).
pub const MAX_TELEPORT_BUDGET_TILES: f32 = MAX_TELEPORT_PER_TICK * 10.0;

/// Player movement (`PlayerMoved`) and entity snapshots are rebroadcast every
/// 3 ticks (20/s).
pub const SNAPSHOT_INTERVAL_TICKS: u32 = 3;

/// `TimeSync` cadence: once per real second.
pub const TIME_SYNC_INTERVAL_TICKS: u32 = 60;

/// `PlayerHeldItem` rebroadcasts triggered by `SelectSlot` are coalesced to
/// at most one per player per this many ticks (the trailing selection still
/// goes out once the window elapses). Without it one inbound frame amplifies
/// into a broadcast to every player at socket speed.
pub const HELD_ITEM_BROADCAST_MIN_TICKS: u64 = SNAPSHOT_INTERVAL_TICKS as u64;

/// Accepted `ToggleDoor` intents are spaced at least this many ticks apart
/// per player — one toggle re-broadcasts a whole door column of tile deltas
/// to every chunk subscriber. 10 ticks ≈ 0.17 s, the fastest §4.1 use time,
/// so legitimate play never notices.
pub const DOOR_TOGGLE_COOLDOWN_TICKS: u64 = 10;

/// Chat messages are stripped of control characters, trimmed, and capped to
/// this many characters.
pub const CHAT_MAX_CHARS: usize = 200;

/// Player names are trimmed and must be 1..=MAX_NAME_CHARS characters.
pub const MAX_NAME_CHARS: usize = 16;

/// A crafting station enables its recipes within this many tiles (§4.4) —
/// deliberately shorter than [`REACH`].
pub const STATION_RANGE: f32 = 4.0;

/// Global cap on live hostile enemies (§0).
pub const MAX_LIVE_ENEMIES: u32 = 200;

/// Hit immunity: players 40 ticks after a hit; enemies 10 ticks per damage
/// source (§0).
pub const PLAYER_IFRAME_TICKS: u32 = 40;
pub const ENEMY_IFRAME_TICKS: u32 = 10;

/// Crits: 4% base chance, ×2 damage (§0).
pub const CRIT_CHANCE: f32 = 0.04;
pub const CRIT_MULT: f32 = 2.0;

/// Player HP (§8): base 100, +20 per Life Crystal, max 400.
pub const PLAYER_BASE_MAX_HP: u32 = 100;
pub const LIFE_CRYSTAL_HP: u32 = 20;
pub const PLAYER_MAX_MAX_HP: u32 = 400;

/// Passive regen (§8): 0.5 HP/s once 8 s have passed without taking damage,
/// doubled while standing still.
pub const REGEN_HP_PER_SEC: f32 = 0.5;
pub const REGEN_DELAY_SECS: f32 = 8.0;
pub const REGEN_STANDING_STILL_MULT: f32 = 2.0;

/// Breath (§8): 200 units; fully submerged it drains 1 unit every 7 ticks
/// (≈23.3 s total); out of water it refills 3 units/tick (≈1.1 s). At 0
/// breath the player takes 10 dmg/s, ignoring defense.
pub const PLAYER_MAX_BREATH: u32 = 200;
pub const BREATH_DRAIN_INTERVAL_TICKS: u32 = 7;
pub const BREATH_REFILL_PER_TICK: u32 = 3;
pub const DROWNING_DPS: u32 = 10;

/// Death & respawn (§8): drop 50% of carried coins where you died (the pile
/// persists 10 min); respawn after 10 s, or 20 s while any boss is alive.
pub const DEATH_COIN_DROP_FRAC: f32 = 0.5;
pub const DEATH_PILE_PERSIST_SECS: u32 = 600;
pub const RESPAWN_SECS: u32 = 10;
pub const RESPAWN_SECS_BOSS_ALIVE: u32 = 20;

/// Debuff magnitudes (§8): Burning ticks 2 dmg/s ignoring defense; Darkness
/// halves the client-computed light radius (§10). Durations are per-source
/// (`tiles::LAVA_BURN_SECS`, `items::POTION_SICKNESS_SECS`, ...); kinds are
/// `protocol::Debuff`.
pub const BURNING_DPS: u32 = 2;
pub const DARKNESS_LIGHT_RADIUS_MULT: f32 = 0.5;

/// Coin denominations (§0): 100 Copper = 1 Silver, etc. Values in code are
/// in copper coins (see `items::ItemData::value`).
pub const COPPER_PER_SILVER: u32 = 100;
pub const COPPER_PER_GOLD: u32 = 10_000;
pub const COPPER_PER_PLATINUM: u32 = 1_000_000;

/// The one damage formula (§0): `max(1, attack − floor(defense / 2))`.
/// Crit doubling and accessory/set multipliers apply to `attack` before
/// calling this.
pub fn damage_dealt(attack: u32, defense: u32) -> u32 {
    (attack as i64 - defense as i64 / 2).max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_formula() {
        assert_eq!(damage_dealt(10, 0), 10);
        assert_eq!(damage_dealt(10, 6), 7);
        assert_eq!(damage_dealt(10, 7), 7); // floor(7/2) = 3
        assert_eq!(damage_dealt(1, 100), 1); // never below 1
        assert_eq!(damage_dealt(0, 0), 1);
    }

    #[test]
    fn tick_constants_agree() {
        assert_eq!((1.0 / DT).round() as u32, TICK_RATE);
    }

    #[test]
    fn reach_is_six_tiles_from_center() {
        let center = (10.5, 10.5); // center of tile (10, 10)
        assert!(tile_in_reach(center, 10, 10));
        assert!(tile_in_reach(center, 16, 10)); // exactly 6 tiles away
        assert!(!tile_in_reach(center, 17, 10)); // 7 tiles away
        assert!(!tile_in_reach(center, 15, 15)); // ~7.07 diagonal
        assert!(tile_in_reach(center, 14, 14)); // ~5.66 diagonal
    }
}
