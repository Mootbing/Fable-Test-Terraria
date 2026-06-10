//! Inventory, crafting, and chest intent handlers — the server-authoritative
//! half of DESIGN §4.4 (crafting) and §8 (inventory layout), plus the §11
//! chest lock rule. Validation and stack semantics live in
//! `shared::inventory_ops` / `shared::crafting`; this module wires them to
//! the sim's players and broadcasts the resulting deltas.

use ferraria_shared::crafting::{recipe_by_id, stations_in_range};
use ferraria_shared::inventory_ops::{apply_slot_op, SlotAddr, SlotDelta, SlotOp};
use ferraria_shared::items::{inventory, InvSlot};
use ferraria_shared::protocol::ServerMessage;
use ferraria_shared::tiles::TileId;
use ferraria_shared::world::{chest_in_reach, CHEST_SLOTS};

use super::game::Sim;

impl Sim {
    // ---- Inventory slot ops (§8) -------------------------------------------

    /// `MoveSlot` / `SplitSlot`: one validated op within the player's own
    /// inventory, answered with `SlotChanged` deltas.
    pub(super) fn inv_slot_op(&mut self, id: u32, from: u8, to: u8, op: SlotOp) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let result = apply_slot_op(
            &mut p.inventory,
            None,
            op,
            SlotAddr::Inv(from as usize),
            SlotAddr::Inv(to as usize),
        );
        match result {
            Ok(deltas) => self.send_slot_deltas(id, &deltas),
            Err(e) => tracing::debug!(player = id, from, to, ?op, ?e, "slot op rejected"),
        }
    }

    /// `DropItem`: removes `count` from `slot`.
    pub(super) fn drop_item(&mut self, id: u32, slot: u8, count: u16) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let idx = slot as usize;
        let Some(stack) = p.inventory.get(idx).copied().flatten() else {
            return;
        };
        if count == 0 || count > stack.count {
            tracing::debug!(player = id, slot, count, "drop with bad count rejected");
            return;
        }
        let left = stack.count - count;
        let new = (left > 0).then_some(InvSlot::new(stack.item, left));
        p.inventory[idx] = new;
        let center = p.center();
        self.send_slot_deltas(id, &[(SlotAddr::Inv(idx), new)]);
        // The stack pops out as a world item drop at the player and is
        // world-shared from then on — first pickup wins (§11), including
        // the dropper after the standard arming delay.
        self.spawn_item_drop(stack.item, count, center);
    }

    // ---- Crafting (§4.4) -----------------------------------------------------

    /// `Craft`: validates the recipe id and station range, consumes inputs
    /// from hotbar+backpack only, stacks the output (overflow drops), and
    /// answers with per-slot deltas.
    pub(super) fn craft(&mut self, id: u32, recipe_id: u16) {
        let Some(recipe) = recipe_by_id(recipe_id) else {
            tracing::debug!(player = id, recipe_id, "unknown recipe");
            return;
        };
        let Some(p) = self.players.get(&id) else {
            return;
        };
        let stations = stations_in_range(&self.world, p.center());
        if !stations.contains(recipe.station) {
            tracing::debug!(player = id, recipe_id, station = ?recipe.station, "station not in range");
            return;
        }
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        // Inputs and outputs touch the hotbar+backpack only — never armor,
        // accessories, or trash (§4.4).
        let crafting = &mut p.inventory[inventory::CRAFTING_SLOTS];
        let before = crafting.to_vec();
        let Some(overflow) =
            ferraria_shared::crafting::apply_craft_overflow(recipe, crafting, true)
        else {
            tracing::debug!(player = id, recipe_id, "missing ingredients");
            return;
        };
        let deltas: Vec<SlotDelta> = before
            .iter()
            .enumerate()
            .filter(|&(i, b)| crafting[i] != *b)
            .map(|(i, _)| (SlotAddr::Inv(i), crafting[i]))
            .collect();
        let output_name = recipe.output.data().name;
        let center = p.center();
        self.send_slot_deltas(id, &deltas);
        if overflow > 0 {
            // §4.4: crafting always yields its output — the part that
            // didn't fit pops out as a world item drop at the crafter.
            self.spawn_item_drop(recipe.output, overflow, center);
            self.send_to(
                id,
                &ServerMessage::Toast {
                    text: format!("Dropped {overflow} {output_name} (inventory full)"),
                },
            );
        }
    }

    // ---- Chests (§2 tile 19, §11 lock rule) -----------------------------------

    /// `OpenChest`: must be a chest origin tile within reach, not held open
    /// by someone else ([`ServerMessage::ChestDenied`]).
    pub(super) fn open_chest(&mut self, id: u32, x: u32, y: u32) {
        if !self.world.in_bounds(x, y) {
            return;
        }
        let t = self.world.tile(x, y);
        if t.id != TileId::Chest || self.world.multitile_origin(x, y) != (x, y) {
            tracing::debug!(player = id, x, y, "open_chest on a non-chest-origin tile");
            return;
        }
        let Some(p) = self.players.get(&id) else {
            return;
        };
        if !chest_in_reach(p.center(), (x, y)) {
            tracing::debug!(player = id, x, y, "chest out of reach");
            return;
        }
        if self
            .chest_locks
            .get(&(x, y))
            .is_some_and(|&holder| holder != id)
        {
            self.send_to(id, &ServerMessage::ChestDenied);
            return;
        }
        self.release_chest(id); // swap from any previously open chest
        self.chest_locks.insert((x, y), id);
        if let Some(p) = self.players.get_mut(&id) {
            p.open_chest = Some((x, y));
        }
        let slots = self
            .world
            .chests
            .entry((x, y))
            .or_insert_with(|| vec![None; CHEST_SLOTS])
            .clone();
        self.send_to(id, &ServerMessage::ChestContents { x, y, slots });
    }

    /// `CloseChest` (also called on disconnect and walk-out-of-reach).
    pub(super) fn close_chest(&mut self, id: u32) {
        self.release_chest(id);
    }

    /// Drops `id`'s chest lock, if any.
    pub(super) fn release_chest(&mut self, id: u32) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if let Some(key) = p.open_chest.take() {
            if self.chest_locks.get(&key) == Some(&id) {
                self.chest_locks.remove(&key);
            }
        }
    }

    /// Per-tick sweep (§11): a chest closes when its opener walks out of
    /// reach. The client applies the same rule locally, so no message is
    /// needed.
    pub(super) fn close_out_of_reach_chests(&mut self) {
        let stale: Vec<u32> = self
            .players
            .iter()
            .filter_map(|(&id, p)| {
                let key = p.open_chest?;
                (!chest_in_reach(p.center(), key)).then_some(id)
            })
            .collect();
        for id in stale {
            self.release_chest(id);
        }
    }

    /// `ChestMoveSlot`: one validated op between the open chest and the
    /// inventory, through the same slot-op engine as `MoveSlot`.
    pub(super) fn chest_move_slot(
        &mut self,
        id: u32,
        chest_slot: u8,
        inv_slot: u8,
        to_chest: bool,
    ) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let Some(key) = p.open_chest else {
            tracing::debug!(player = id, "chest move with no chest open");
            return;
        };
        let Some(chest) = self.world.chests.get_mut(&key) else {
            tracing::warn!(player = id, ?key, "open chest has no contents entry");
            return;
        };
        let (from, to) = if to_chest {
            (
                SlotAddr::Inv(inv_slot as usize),
                SlotAddr::Chest(chest_slot as usize),
            )
        } else {
            (
                SlotAddr::Chest(chest_slot as usize),
                SlotAddr::Inv(inv_slot as usize),
            )
        };
        match apply_slot_op(&mut p.inventory, Some(chest), SlotOp::Move, from, to) {
            Ok(deltas) => self.send_slot_deltas(id, &deltas),
            Err(e) => {
                tracing::debug!(
                    player = id,
                    chest_slot,
                    inv_slot,
                    to_chest,
                    ?e,
                    "chest op rejected"
                )
            }
        }
    }

    // ---- Shared plumbing -------------------------------------------------------

    /// Sends slot deltas to their owner (`SlotChanged` for inventory slots,
    /// `ChestSlotChanged` for the open chest) and rebroadcasts the held item
    /// if the held hotbar slot was among them.
    fn send_slot_deltas(&mut self, id: u32, deltas: &[SlotDelta]) {
        let held_changed = self.players.get(&id).is_some_and(|p| {
            deltas
                .iter()
                .any(|&(addr, _)| addr == SlotAddr::Inv(p.held_slot as usize))
        });
        for &(addr, stack) in deltas {
            let msg = match addr {
                SlotAddr::Inv(i) => ServerMessage::SlotChanged {
                    idx: i as u8,
                    stack,
                },
                SlotAddr::Chest(i) => ServerMessage::ChestSlotChanged {
                    idx: i as u8,
                    stack,
                },
            };
            self.send_to(id, &msg);
        }
        if held_changed {
            self.broadcast_held(id);
        }
    }

    /// Tells everyone else what `id` now holds (their own client already
    /// knows from the `SlotChanged`).
    fn broadcast_held(&mut self, id: u32) {
        let Some(p) = self.players.get(&id) else {
            return;
        };
        let msg = ServerMessage::PlayerHeldItem {
            id,
            slot: p.held_slot,
            item: p.held_item(),
        };
        let frame: super::game::Frame = ferraria_shared::protocol::encode(&msg).into();
        self.broadcast_frame(&frame, Some(id));
    }
}

#[cfg(test)]
mod tests {
    use super::super::game::testutil::*;
    use super::super::game::{Frame, Sim};
    use super::*;
    use ferraria_shared::items::ItemId;
    use ferraria_shared::protocol::ClientMessage;
    use ferraria_shared::{loadout, physics};
    use tokio::sync::mpsc;

    fn give(sim: &mut Sim, id: u32, slot: usize, item: ItemId, count: u16) {
        sim.players.get_mut(&id).expect("player").inventory[slot] = Some(InvSlot::new(item, count));
    }

    fn slot(sim: &Sim, id: u32, idx: usize) -> Option<InvSlot> {
        sim.players[&id].inventory[idx]
    }

    fn slot_changes(msgs: &[ServerMessage]) -> Vec<(u8, Option<InvSlot>)> {
        msgs.iter()
            .filter_map(|m| match m {
                ServerMessage::SlotChanged { idx, stack } => Some((*idx, *stack)),
                _ => None,
            })
            .collect()
    }

    fn chest_changes(msgs: &[ServerMessage]) -> Vec<(u8, Option<InvSlot>)> {
        msgs.iter()
            .filter_map(|m| match m {
                ServerMessage::ChestSlotChanged { idx, stack } => Some((*idx, *stack)),
                _ => None,
            })
            .collect()
    }

    /// Sim + one joined player with a drained queue.
    fn sim_with_player() -> (Sim, u32, u64, mpsc::Receiver<Frame>) {
        let mut sim = test_sim();
        let (id, epoch, mut rx) = join(&mut sim, "alice", None);
        drain(&mut rx);
        (sim, id, epoch, rx)
    }

    #[test]
    fn move_and_split_emit_slot_deltas() {
        let (mut sim, id, epoch, mut rx) = sim_with_player();
        // Starting kit: sword in 0, torches ×5 in 3.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::MoveSlot { from: 0, to: 15 },
        );
        let changes = slot_changes(&drain(&mut rx));
        assert!(changes.contains(&(0, None)));
        assert!(changes.contains(&(15, Some(InvSlot::new(ItemId::WoodSword, 1)))));

        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::SplitSlot { from: 3, to: 20 },
        );
        let changes = slot_changes(&drain(&mut rx));
        assert!(changes.contains(&(3, Some(InvSlot::new(ItemId::Torch, 2)))));
        assert!(changes.contains(&(20, Some(InvSlot::new(ItemId::Torch, 3)))));

        // Garbage ops change nothing and answer nothing.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::MoveSlot { from: 30, to: 31 },
        );
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::MoveSlot { from: 0, to: 200 },
        );
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn equipping_through_messages_updates_loadout_math() {
        let (mut sim, id, epoch, mut rx) = sim_with_player();
        give(&mut sim, id, 10, ItemId::IronHelmet, 1);
        give(&mut sim, id, 11, ItemId::SwiftBoots, 1);
        give(&mut sim, id, 12, ItemId::SwiftBoots, 1);

        let head = inventory::ARMOR_START as u8;
        let acc = inventory::ACCESSORY_START as u8;
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::MoveSlot { from: 10, to: head },
        );
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::MoveSlot { from: 11, to: acc },
        );
        let inv = sim.players[&id].inventory.clone();
        assert_eq!(loadout::defense(&inv), 2, "iron helmet equipped");
        assert_eq!(
            loadout::physics_mods(&inv).speed_mult,
            ferraria_shared::items::SWIFT_BOOTS_SPEED_MULT
        );
        drain(&mut rx);

        // Duplicate accessory and wrong armor slot are rejected server-side.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::MoveSlot {
                from: 12,
                to: acc + 1,
            },
        );
        assert_eq!(
            slot(&sim, id, 12),
            Some(InvSlot::new(ItemId::SwiftBoots, 1))
        );
        give(&mut sim, id, 13, ItemId::IronGreaves, 1);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::MoveSlot { from: 13, to: head },
        );
        assert_eq!(
            slot(&sim, id, 13),
            Some(InvSlot::new(ItemId::IronGreaves, 1))
        );
        assert!(drain(&mut rx).is_empty(), "rejections are silent");

        // Physics agrees with the loadout: a mid-air jump works only with
        // the Gust Jar equipped (server-side clamp source of truth).
        let mods = loadout::physics_mods(&sim.players[&id].inventory);
        assert_eq!(mods.extra_air_jumps, 0);
        give(&mut sim, id, 14, ItemId::GustJar, 1);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::MoveSlot {
                from: 14,
                to: acc + 1,
            },
        );
        let mods = loadout::physics_mods(&sim.players[&id].inventory);
        assert_eq!(
            mods,
            physics::PhysicsMods {
                speed_mult: ferraria_shared::items::SWIFT_BOOTS_SPEED_MULT,
                extra_air_jumps: 1,
                no_fall_damage: false
            }
        );
    }

    #[test]
    fn moving_the_held_slot_rebroadcasts_the_held_item() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        let (_b, _, mut rx_b) = join(&mut sim, "bob", None);
        drain(&mut rx_a);
        drain(&mut rx_b);
        // Alice holds slot 0 (wood sword); moving it away empties her hand.
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::MoveSlot { from: 0, to: 9 },
        );
        let held = drain(&mut rx_b)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::PlayerHeldItem { id, slot, item } if id == a => Some((slot, item)),
                _ => None,
            })
            .expect("held item rebroadcast");
        assert_eq!(held, (0, None));
    }

    #[test]
    fn craft_validates_station_inputs_and_emits_deltas() {
        let (mut sim, id, epoch, mut rx) = sim_with_player();
        give(&mut sim, id, 10, ItemId::Wood, 12);

        // Recipe 4 (platforms) needs a workbench: none nearby -> rejected.
        msg(&mut sim, id, epoch, ClientMessage::Craft { recipe_id: 4 });
        assert!(drain(&mut rx).is_empty());
        assert_eq!(slot(&sim, id, 10), Some(InvSlot::new(ItemId::Wood, 12)));

        // Recipe 1 (workbench) crafts at Hands, anywhere.
        msg(&mut sim, id, epoch, ClientMessage::Craft { recipe_id: 1 });
        let changes = slot_changes(&drain(&mut rx));
        assert!(changes.contains(&(10, Some(InvSlot::new(ItemId::Wood, 2)))));
        assert!(changes
            .iter()
            .any(|&(_, s)| s == Some(InvSlot::new(ItemId::Workbench, 1))));

        // Unknown recipe id and missing ingredients are silent no-ops.
        msg(&mut sim, id, epoch, ClientMessage::Craft { recipe_id: 999 });
        msg(&mut sim, id, epoch, ClientMessage::Craft { recipe_id: 2 }); // torch needs gel
        assert!(drain(&mut rx).is_empty());

        // Place a workbench tile next to the player: recipe 4 now works.
        let center = sim.players[&id].center();
        let (bx, by) = (center.0 as u32 + 1, center.1 as u32);
        assert!(sim.world.place_multitile(bx, by, TileId::Workbench));
        msg(&mut sim, id, epoch, ClientMessage::Craft { recipe_id: 4 });
        let changes = slot_changes(&drain(&mut rx));
        assert!(changes.contains(&(10, Some(InvSlot::new(ItemId::Wood, 1)))));
        assert!(changes
            .iter()
            .any(|&(_, s)| s == Some(InvSlot::new(ItemId::Platform, 2))));
    }

    #[test]
    fn craft_consumes_exactly_across_split_stacks_and_stacks_output() {
        let (mut sim, id, epoch, mut rx) = sim_with_player();
        // Torch recipe (#2, Hands): 1 wood + 1 gel -> 3 torches, onto the
        // starting-kit torch stack (5 in slot 3). Wood split across two
        // stacks: the first is drained first.
        give(&mut sim, id, 10, ItemId::Wood, 1);
        give(&mut sim, id, 11, ItemId::Wood, 4);
        give(&mut sim, id, 12, ItemId::Gel, 2);
        msg(&mut sim, id, epoch, ClientMessage::Craft { recipe_id: 2 });
        let changes = slot_changes(&drain(&mut rx));
        assert!(changes.contains(&(10, None)), "first wood stack emptied");
        assert!(changes.contains(&(12, Some(InvSlot::new(ItemId::Gel, 1)))));
        assert!(
            changes.contains(&(3, Some(InvSlot::new(ItemId::Torch, 8)))),
            "output stacked onto the existing torches: {changes:?}"
        );
        assert_eq!(slot(&sim, id, 11), Some(InvSlot::new(ItemId::Wood, 4)));

        // Crafting never touches armor/accessory/trash stacks.
        give(&mut sim, id, inventory::ARMOR_START, ItemId::IronHelmet, 1);
        give(&mut sim, id, inventory::TRASH, ItemId::Gel, 50);
        give(&mut sim, id, 12, ItemId::Gel, 1);
        give(&mut sim, id, 13, ItemId::Wood, 1);
        drain(&mut rx);
        msg(&mut sim, id, epoch, ClientMessage::Craft { recipe_id: 2 });
        drain(&mut rx);
        assert_eq!(slot(&sim, id, 12), None, "gel consumed from backpack");
        assert_eq!(
            slot(&sim, id, inventory::TRASH),
            Some(InvSlot::new(ItemId::Gel, 50)),
            "trash gel untouched"
        );
    }

    #[test]
    fn chest_lock_denies_second_player_until_closed() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        let (b, b_epoch, mut rx_b) = join(&mut sim, "bob", None);
        drain(&mut rx_a);
        drain(&mut rx_b);
        let center = sim.players[&a].center();
        let (cx, cy) = (center.0 as u32 + 2, center.1 as u32);
        assert!(sim.world.place_multitile(cx, cy, TileId::Chest));

        // Opening a non-origin cell is rejected silently.
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::OpenChest { x: cx + 1, y: cy },
        );
        assert!(drain(&mut rx_a).is_empty());

        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::OpenChest { x: cx, y: cy },
        );
        let contents = drain(&mut rx_a);
        assert!(
            contents.iter().any(|m| matches!(m,
                ServerMessage::ChestContents { x, y, slots }
                    if *x == cx && *y == cy && slots.len() == CHEST_SLOTS)),
            "opener gets the contents: {contents:?}"
        );

        // Second player: denied while held open.
        msg(
            &mut sim,
            b,
            b_epoch,
            ClientMessage::OpenChest { x: cx, y: cy },
        );
        assert!(drain(&mut rx_b)
            .iter()
            .any(|m| matches!(m, ServerMessage::ChestDenied)));

        // Takeover after close.
        msg(&mut sim, a, a_epoch, ClientMessage::CloseChest);
        msg(
            &mut sim,
            b,
            b_epoch,
            ClientMessage::OpenChest { x: cx, y: cy },
        );
        assert!(drain(&mut rx_b)
            .iter()
            .any(|m| matches!(m, ServerMessage::ChestContents { .. })));
    }

    #[test]
    fn chest_moves_update_both_sides_and_close_on_walkaway() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        let (b, b_epoch, mut rx_b) = join(&mut sim, "bob", None);
        drain(&mut rx_a);
        drain(&mut rx_b);
        let center = sim.players[&a].center();
        let (cx, cy) = (center.0 as u32 + 2, center.1 as u32);
        assert!(sim.world.place_multitile(cx, cy, TileId::Chest));
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::OpenChest { x: cx, y: cy },
        );
        drain(&mut rx_a);

        // Deposit the starting torches (slot 3) into chest slot 0.
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::ChestMoveSlot {
                chest_slot: 0,
                inv_slot: 3,
                to_chest: true,
            },
        );
        let msgs = drain(&mut rx_a);
        assert!(slot_changes(&msgs).contains(&(3, None)));
        assert!(chest_changes(&msgs).contains(&(0, Some(InvSlot::new(ItemId::Torch, 5)))));

        // Without an open chest the op is rejected (bob).
        msg(
            &mut sim,
            b,
            b_epoch,
            ClientMessage::ChestMoveSlot {
                chest_slot: 0,
                inv_slot: 0,
                to_chest: true,
            },
        );
        assert!(drain(&mut rx_b).is_empty());

        // Walking out of reach closes the lock on the next tick; bob can
        // then open the chest and sees the deposited torches.
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::PlayerState {
                pos: (center.0 + 20.0, center.1),
                vel: (0.0, 0.0),
                facing: 1,
                anim: 0,
            },
        );
        sim.tick();
        assert!(sim.players[&a].open_chest.is_none(), "lock released");
        msg(
            &mut sim,
            b,
            b_epoch,
            ClientMessage::OpenChest { x: cx, y: cy },
        );
        let torches = drain(&mut rx_b)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::ChestContents { slots, .. } => Some(slots[0]),
                _ => None,
            })
            .expect("bob opened after walk-away");
        assert_eq!(torches, Some(InvSlot::new(ItemId::Torch, 5)));

        // Withdraw back out (bob).
        msg(
            &mut sim,
            b,
            b_epoch,
            ClientMessage::ChestMoveSlot {
                chest_slot: 0,
                inv_slot: 9,
                to_chest: false,
            },
        );
        let msgs = drain(&mut rx_b);
        assert!(slot_changes(&msgs).contains(&(9, Some(InvSlot::new(ItemId::Torch, 5)))));
        assert!(chest_changes(&msgs).contains(&(0, None)));
    }

    #[test]
    fn disconnect_releases_the_chest_lock() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        let (b, b_epoch, mut rx_b) = join(&mut sim, "bob", None);
        drain(&mut rx_a);
        drain(&mut rx_b);
        let center = sim.players[&a].center();
        let (cx, cy) = (center.0 as u32 + 2, center.1 as u32);
        assert!(sim.world.place_multitile(cx, cy, TileId::Chest));
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::OpenChest { x: cx, y: cy },
        );
        sim.handle(crate::sim::game::SimCommand::Disconnect {
            player_id: a,
            epoch: a_epoch,
        });
        assert!(sim.chest_locks.is_empty(), "disconnect freed the lock");
        msg(
            &mut sim,
            b,
            b_epoch,
            ClientMessage::OpenChest { x: cx, y: cy },
        );
        assert!(drain(&mut rx_b)
            .iter()
            .any(|m| matches!(m, ServerMessage::ChestContents { .. })));
    }

    #[test]
    fn breaking_an_open_empty_chest_releases_the_lock() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        drain(&mut rx_a);
        let center = sim.players[&a].center();
        let (cx, cy) = (center.0 as u32 + 2, center.1 as u32);
        assert!(sim.world.place_multitile(cx, cy, TileId::Chest));
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::OpenChest { x: cx, y: cy },
        );
        assert!(!sim.chest_locks.is_empty());

        sim.break_tile(cx, cy);
        assert_eq!(sim.world.tile(cx, cy).id, TileId::Air, "empty chest broke");
        assert!(sim.chest_locks.is_empty(), "the break released the lock");
        assert!(sim.players[&a].open_chest.is_none());
    }

    #[test]
    fn craft_overflow_spawns_an_item_drop() {
        let (mut sim, id, epoch, mut rx) = sim_with_player();
        // Recipe 2 (torch ×3 at Hands) with no room for the output: every
        // crafting slot full, and the inputs don't empty a slot.
        {
            let p = sim.players.get_mut(&id).expect("player");
            for i in inventory::CRAFTING_SLOTS {
                p.inventory[i] = Some(InvSlot::new(ItemId::Stone, ItemId::Stone.max_stack()));
            }
            p.inventory[0] = Some(InvSlot::new(ItemId::Wood, 2));
            p.inventory[1] = Some(InvSlot::new(ItemId::Gel, 2));
        }
        msg(&mut sim, id, epoch, ClientMessage::Craft { recipe_id: 2 });
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                ServerMessage::ItemDropSpawn {
                    item: ItemId::Torch,
                    count: 3,
                    ..
                }
            )),
            "the whole output overflowed into a world drop: {msgs:?}"
        );
        assert!(msgs.iter().any(
            |m| matches!(m, ServerMessage::Toast { text } if text.contains("inventory full"))
        ));
        // Inputs were still consumed (§4.4: crafting always yields).
        assert_eq!(slot(&sim, id, 0), Some(InvSlot::new(ItemId::Wood, 1)));
        assert_eq!(slot(&sim, id, 1), Some(InvSlot::new(ItemId::Gel, 1)));
    }

    #[test]
    fn drop_item_validates_slot_and_count() {
        let (mut sim, id, epoch, mut rx) = sim_with_player();
        // Bad counts: nothing happens.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::DropItem { slot: 3, count: 0 },
        );
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::DropItem { slot: 3, count: 6 },
        );
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::DropItem { slot: 20, count: 1 },
        );
        assert!(drain(&mut rx).is_empty());
        assert_eq!(slot(&sim, id, 3), Some(InvSlot::new(ItemId::Torch, 5)));

        // Valid drop removes the items and spawns a world item-drop entity
        // at the player (world-shared, first pickup wins).
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::DropItem { slot: 3, count: 2 },
        );
        let msgs = drain(&mut rx);
        assert!(slot_changes(&msgs).contains(&(3, Some(InvSlot::new(ItemId::Torch, 3)))));
        let center = sim.players[&id].center();
        let pos = msgs
            .iter()
            .find_map(|m| match m {
                ServerMessage::ItemDropSpawn {
                    item: ItemId::Torch,
                    count: 2,
                    pos,
                    ..
                } => Some(*pos),
                _ => None,
            })
            .expect("dropping spawned an item-drop entity");
        assert!(
            (pos.0 - center.0).abs() < 2.0 && (pos.1 - center.1).abs() < 2.0,
            "drop spawns at the player, got {pos:?} vs {center:?}"
        );

        // Dropping the whole stack empties the slot.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::DropItem { slot: 3, count: 3 },
        );
        assert_eq!(slot(&sim, id, 3), None);
    }

    #[test]
    fn chest_reach_rule() {
        // Any cell of the 2×2 footprint within REACH counts.
        assert!(chest_in_reach((10.0, 10.0), (12, 10)));
        assert!(
            chest_in_reach((10.0, 10.0), (15, 10)),
            "far cell at 6.04, near at 5.5"
        );
        assert!(!chest_in_reach((10.0, 10.0), (17, 10)));
        assert!(!chest_in_reach((10.0, 10.0), (12, 18)));
    }
}
