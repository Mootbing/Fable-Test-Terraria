//! Types shared between the Ferraria server and client: tile/item/world data
//! tables and model, deterministic RNG, player physics, crafting, and the
//! WebSocket wire protocol.
//!
//! No tokio, no macroquad, no I/O — everything here compiles for both native
//! and wasm32-unknown-unknown and is pure/deterministic (ARCHITECTURE.md).
//! Gameplay numbers live in the data tables and constants, sourced from
//! DESIGN.md; don't inline them elsewhere.

pub mod crafting;
pub mod items;
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
pub const PROTOCOL_VERSION: u32 = 1;

/// World tiles are 16x16 px on screen; physics positions are in tile units.
pub const TILE_SIZE: f32 = 16.0;

/// Simulation rate, ticks per second.
pub const TICK_RATE: u32 = 60;

/// Seconds per tick. Velocities are tiles/second; multiply by `DT` per tick.
pub const DT: f32 = 1.0 / TICK_RATE as f32;

/// Players can mine/place/interact within this many tiles of their center.
pub const REACH: f32 = 6.0;

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
}
