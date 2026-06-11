//! Server-side simulation systems: the 60 tps game loop ([`game`]), the
//! fluid automaton ([`fluids`]), the mine/place/build intents ([`interact`]),
//! the dropped-item entities ([`entities`]), the inventory/crafting/chest
//! intent handlers ([`inventory`]), and the live world ticks — fluids
//! cadence, falling sand, grass/sapling random ticks ([`world_tick`]).
//! Enemies, combat, and the rest of the tick pipeline arrive with later PRs
//! and plug into [`game::Sim::tick`].

pub mod entities;
pub mod fluids;
pub mod game;
pub mod housing;
pub mod interact;
mod inventory;
pub mod npc;
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
    use ferraria_shared::rng::Pcg32;
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
        let mut sim = Sim::new(world, Arc::new(AtomicUsize::new(0)));
        // Entropy seeding belongs to production boots; the test suite must
        // replay identically run-to-run (NPC wander walks, loot rolls).
        sim.loot_rng = Pcg32::new(0xF3AA_11A5);
        sim
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
