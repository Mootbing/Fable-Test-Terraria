//! Server-side simulation systems: the 60 tps game loop ([`game`]), the
//! fluid automaton ([`fluids`]), the mine/place/build intents ([`interact`]),
//! the dropped-item entities ([`entities`]), the inventory/crafting/chest
//! intent handlers ([`inventory`]), and the live world ticks — fluids
//! cadence, falling sand, grass/sapling random ticks ([`world_tick`]).
//! Enemies, combat, and the rest of the tick pipeline arrive with later PRs
//! and plug into [`game::Sim::tick`].

pub mod combat;
pub mod enemies;
pub mod entities;
pub mod fluids;
pub mod game;
pub mod interact;
mod inventory;
pub mod survival;
pub mod world_tick;

/// Shared helpers for the sim test suites (driving a [`game::Sim`] without
/// the tokio interval or real sockets).
#[cfg(test)]
pub(crate) mod test_util {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    use tokio::sync::{mpsc, oneshot};

    use ferraria_shared::items::{InvSlot, ItemId};
    use ferraria_shared::physics::{PLAYER_HEIGHT, PLAYER_WIDTH};
    use ferraria_shared::protocol::{decode, ClientMessage, ServerMessage};
    use ferraria_shared::tiles::{Tile, TileId};
    use ferraria_shared::world::World;

    use super::game::{Frame, Sim, SimCommand, OUTBOUND_QUEUE_FRAMES};

    /// A sim over a `width`×`height` world that is all air except a solid
    /// stone floor whose top row is `floor_y`. Spawn sits on the floor at
    /// the horizontal center.
    pub fn flat_sim(width: u32, height: u32, floor_y: u32) -> Sim {
        let mut world = World::new(width, height);
        for y in floor_y..height {
            for x in 0..width {
                world.set_tile(x, y, Tile::of(TileId::Stone));
            }
        }
        world.spawn = (width / 2, floor_y - 1);
        Sim::new(world, Arc::new(AtomicUsize::new(0)))
    }

    /// Joins a player; returns their id, session epoch, and frame receiver.
    pub fn join(sim: &mut Sim, name: &str) -> (u32, u64, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_FRAMES);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        sim.handle(SimCommand::Join {
            name: name.into(),
            token: None,
            tx,
            reply: reply_tx,
        });
        let (id, epoch) = reply_rx
            .try_recv()
            .expect("sim replied")
            .expect("join accepted");
        (id, epoch, rx)
    }

    pub fn msg(sim: &mut Sim, player_id: u32, epoch: u64, msg: ClientMessage) {
        sim.handle(SimCommand::Message {
            player_id,
            epoch,
            msg,
        });
    }

    pub fn drain(rx: &mut mpsc::Receiver<Frame>) -> Vec<ServerMessage> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(decode::<ServerMessage>(&frame).expect("valid frame"));
        }
        out
    }

    /// Puts `count` of `item` in hotbar `slot` and selects it.
    pub fn give(sim: &mut Sim, id: u32, slot: u8, item: ItemId, count: u16) {
        let p = sim.players.get_mut(&id).expect("player");
        p.inventory[slot as usize] = Some(InvSlot::new(item, count));
        p.held_slot = slot;
    }

    /// Empties the player's hands (and the selected slot).
    pub fn give_nothing(sim: &mut Sim, id: u32, slot: u8) {
        let p = sim.players.get_mut(&id).expect("player");
        p.inventory[slot as usize] = None;
        p.held_slot = slot;
    }

    /// Teleports the player so their feet-center stands on top of tile row
    /// `y_feet` at column center `x`.
    pub fn place_player(sim: &mut Sim, id: u32, x: f32, y_feet: f32) {
        let p = sim.players.get_mut(&id).expect("player");
        p.pos = (x - PLAYER_WIDTH / 2.0, y_feet - PLAYER_HEIGHT - 1e-3);
    }

    /// Runs `n` sim ticks.
    pub fn advance(sim: &mut Sim, n: u32) {
        for _ in 0..n {
            sim.tick();
        }
    }

    /// Swings at `(x, y)` then waits long enough for any §4.1 swing
    /// cooldown (the slowest use time is 0.55 s = 33 ticks).
    pub fn swing_tile(sim: &mut Sim, id: u32, epoch: u64, x: u32, y: u32) {
        msg(sim, id, epoch, ClientMessage::HitTile { x, y });
        advance(sim, 40);
    }
}

/// End-to-end flows through the full sim pipeline (intent dispatch + tick
/// systems + broadcast frames) — the §5/§8 analog of `tests/netplay.rs`.
/// (Driving these over a real WebSocket isn't practical: natural spawns
/// land 62–84 tiles out by design and would take minutes of real 60 tps
/// time to walk into contact range.)
#[cfg(test)]
mod integration {
    use super::test_util::*;
    use ferraria_shared::enemies::{EnemyKind, SpawnEnvironment};
    use ferraria_shared::items::{InvSlot, ItemId};
    use ferraria_shared::protocol::{ClientMessage, DespawnReason, ServerMessage};
    use ferraria_shared::{RESPAWN_SECS, TICK_RATE};

    const FLOOR: u32 = 40;

    #[test]
    fn spawned_slime_damages_player_to_death_then_respawn() {
        let mut sim = flat_sim(120, 80, FLOOR);
        sim.world.time = 0; // night: slimes are hostile
        let (id, epoch, mut rx) = join(&mut sim, "victim");
        drain(&mut rx);
        place_player(&mut sim, id, 60.0, FLOOR as f32);
        sim.players.get_mut(&id).expect("p").hp = 20; // two slime hits
        sim.players.get_mut(&id).expect("p").inventory[5] =
            Some(InvSlot::new(ItemId::SilverCoin, 10));

        // A hostile green slime dropped onto the player's head.
        sim.spawn_enemy(
            EnemyKind::GreenSlime,
            (59.5, FLOOR as f32 - 3.0),
            SpawnEnvironment::SurfaceNight,
        );
        // Contact damage (6 vs 0 defense) lands within a few ticks, then
        // the §0 i-frames (40 ticks) gate the second hit; two hits kill.
        let mut died_at = None;
        for t in 0..400u32 {
            advance(&mut sim, 1);
            if sim.players[&id].dead {
                died_at = Some(t);
                break;
            }
        }
        assert!(died_at.is_some(), "slime contact killed the player");
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::PlayerHealth { id: pid, hp: 14, .. } if *pid == id)));
        assert!(msgs
            .iter()
            .any(|m| matches!(m, ServerMessage::PlayerKnockback { .. })));
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::PlayerDied { id: pid } if *pid == id)));
        // §8: half the carried coins dropped at the corpse.
        assert_eq!(
            sim.players[&id].inventory[5],
            Some(InvSlot::new(ItemId::SilverCoin, 5))
        );

        // Respawn after the 10 s timer at the world spawn with full base HP.
        advance(&mut sim, RESPAWN_SECS * TICK_RATE + 1);
        msg(&mut sim, id, epoch, ClientMessage::Respawn);
        let p = &sim.players[&id];
        assert!(!p.dead);
        assert_eq!(p.hp, 100);
        assert!(drain(&mut rx).iter().any(|m| matches!(m,
            ServerMessage::PlayerRespawned { id: pid, .. } if *pid == id)));
    }

    #[test]
    fn melee_kill_yields_gel_drop_and_pickup() {
        let mut sim = flat_sim(120, 80, FLOOR);
        sim.world.time = 0;
        let (id, epoch, mut rx) = join(&mut sim, "slayer");
        place_player(&mut sim, id, 60.0, FLOOR as f32);
        give(&mut sim, id, 0, ItemId::GoldSword, 1); // 16 dmg: one-shots 14 HP
                                                     // Clear backpack noise so the gel pickup is easy to spot.
        {
            let p = sim.players.get_mut(&id).expect("p");
            for i in 1..10 {
                p.inventory[i] = None;
            }
        }
        let eid = sim.spawn_enemy(
            EnemyKind::GreenSlime,
            (61.5, FLOOR as f32 - 1.5),
            SpawnEnvironment::SurfaceNight,
        );
        drain(&mut rx);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::UseItem {
                slot: 0,
                aim: (62.0, FLOOR as f32 - 1.0),
            },
        );
        advance(&mut sim, 2);
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::EntityHurt { id, .. } if *id == eid)));
        assert!(
            msgs.iter().any(|m| matches!(m,
                ServerMessage::EntityDespawn { id, reason: DespawnReason::Killed } if *id == eid)),
            "death poof broadcast"
        );
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerMessage::ItemDropSpawn {
                    item: ItemId::Gel,
                    ..
                }
            )),
            "§5.1: slimes always drop gel"
        );
        // Step onto the loot pile; the drops arm (0.5 s) and vacuum up.
        place_player(&mut sim, id, 62.0, FLOOR as f32);
        advance(&mut sim, 60);
        let msgs = drain(&mut rx);
        assert!(msgs
            .iter()
            .any(|m| matches!(m, ServerMessage::ItemPickedUp { by, .. } if *by == id)));
        let gel: u16 = sim.players[&id]
            .inventory
            .iter()
            .flatten()
            .filter(|s| s.item == ItemId::Gel)
            .map(|s| s.count)
            .sum();
        assert!((1..=2).contains(&gel), "picked up the §5.1 gel ({gel})");
        // Only check *this* slime: it's night and natural spawning is live,
        // so a fresh enemy may legitimately roll in during the ~62 ticks
        // above (§5.2 SurfaceNight chance per player per tick).
        assert!(!sim.entities.map.contains_key(&eid), "slime is gone");
    }
}
