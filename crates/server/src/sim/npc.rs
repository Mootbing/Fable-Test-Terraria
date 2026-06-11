//! Town NPCs (DESIGN §7): lifecycle (arrival, housing claims, dawn
//! respawns), day/night AI, dialogue, the merchant shop, nurse healing
//! (§7.4), and personal bed spawns (§8).
//!
//! Lifecycle notes (canonized choices, called out per the task spec):
//! - The Sage spawns at the world spawn on boot. Sim state isn't persisted
//!   yet (the persistence PR), so every boot is "first world boot".
//! - Arriving/respawning NPCs **spawn at their claimed house's home tile**
//!   (the simpler §7.2 option vs. walking in from off-screen).
//! - Door pass-through is a **simple teleport-through**: a walking NPC
//!   blocked by a closed door is moved to the other side; the door never
//!   visibly opens (allowed by the task spec — "NPCs may open+close doors
//!   as they pass — simple teleport-through is fine").
//! - At night or while a boss is alive, an NPC away from its home tile
//!   teleports home once and stands there (server-side pathfinding through
//!   arbitrary player builds is out of scope for v1).

use std::collections::BTreeMap;

use ferraria_shared::coins::{add_coins, coin_total, remove_coins};
use ferraria_shared::items::{add_to_inventory, inventory, InvSlot, ItemId};
use ferraria_shared::npc::{
    anim as npc_anim, npc_data, nurse_heal_cost, pick_line, sell_price, shop_price,
    ArrivalCondition, DialogueCtx, LOW_HP_PERCENT, MERCHANT_SHOP, NPC_DEFENSE,
    NPC_FIGHT_BACK_DAMAGE, NPC_HP, NPC_KINDS, NPC_TALK_RANGE, NPC_WANDER_RADIUS,
    RICH_PLAYER_COPPER,
};
use ferraria_shared::physics::{
    step_player_with_mods, PhysicsMods, PlayerInput, PlayerPhysics, PLAYER_HEIGHT, PLAYER_WIDTH,
};
use ferraria_shared::protocol::{
    ActiveDebuff, Debuff, DespawnReason, NpcInfo, NpcKind, ServerMessage, ShopEntry,
};
use ferraria_shared::rng::Pcg32;
use ferraria_shared::tiles::TileId;
use ferraria_shared::world::{World, DAWN_TICK};
use ferraria_shared::{damage_dealt, DT};

use super::entities::{Entity, EntityKind};
use super::game::Sim;
use super::housing::{check_house, find_vacant_house};

/// Arrival conditions are evaluated at dawn AND on every in-game hour (§7.2
/// "checked on demand"; 1 in-game hour = 3600 ticks = 60 real seconds).
pub const IN_GAME_HOUR_TICKS: u32 = 3600;

/// NPC walk speed as a fraction of the player run max (≈2 t/s) — town folk
/// stroll. DESIGN gives no number; canonized here.
const NPC_WALK_SPEED_MULT: f32 = 0.18;
/// Wander behavior re-roll bounds, in ticks.
const WANDER_WALK_TICKS: (u32, u32) = (120, 420);
const WANDER_PAUSE_TICKS: (u32, u32) = (60, 240);
/// Turn around rather than walk off a drop deeper than this many tiles.
const LEDGE_MAX_DROP: u32 = 3;

// ---- Debuff storage seam ------------------------------------------------------

/// Per-player debuff storage.
///
/// INTEGRATE(nurse-debuffs): the enemies/combat branch owns debuff
/// *application and ticking* (Burning damage, Darkness, Potion Sickness on
/// potion use). This branch only needs storage the Nurse can count and
/// clear (§7.4) and dialogue can read; nothing on this branch inserts
/// debuffs. The merge train should keep exactly one storage — this one or
/// the sibling's — and point both features at it.
#[derive(Debug, Default, Clone)]
pub struct DebuffSet {
    active: Vec<ActiveDebuff>,
}

impl DebuffSet {
    pub fn has(&self, debuff: Debuff) -> bool {
        self.active.iter().any(|d| d.debuff == debuff)
    }

    /// How many active debuffs a Nurse heal would clear (everything except
    /// Potion Sickness, §7.4).
    pub fn nurse_clearable(&self) -> u32 {
        self.active
            .iter()
            .filter(|d| d.debuff != Debuff::PotionSickness)
            .count() as u32
    }

    /// §7.4: clears everything except Potion Sickness.
    pub fn clear_except_potion_sickness(&mut self) {
        self.active.retain(|d| d.debuff == Debuff::PotionSickness);
    }

    /// Wire form for `ServerMessage::PlayerDebuffs`.
    pub fn as_wire(&self) -> Vec<ActiveDebuff> {
        self.active.clone()
    }

    #[cfg(test)]
    pub fn insert_for_test(&mut self, debuff: Debuff, remaining_ticks: u32) {
        self.active.push(ActiveDebuff {
            debuff,
            remaining_ticks,
        });
    }
}

// ---- NPC state ----------------------------------------------------------------

/// Per-NPC state, keyed by the NPC's entity id (the [`EntityStore`] entry
/// carries the shared pos/vel for snapshots/visibility).
///
/// [`EntityStore`]: super::entities::EntityStore
pub(crate) struct Npc {
    pub kind: NpcKind,
    pub hp: u32,
    /// Home tile (the §7.1 1×3 column's feet cell); `None` = homeless.
    pub home: Option<(u32, u32)>,
    /// Authoritative physics body; mirrored into the entity store per tick.
    phys: PlayerPhysics,
    /// Walk direction, ±1.
    dir: i8,
    /// Ticks left standing still (wander pause).
    pause: u32,
    /// Ticks left walking before the behavior re-rolls.
    walk: u32,
}

impl Npc {
    fn center(&self) -> (f32, f32) {
        self.phys.center()
    }
}

/// Town NPC roster bookkeeping that lives on [`Sim`].
#[derive(Default)]
pub(crate) struct TownState {
    /// Per-NPC state by entity id (`BTreeMap` for deterministic order).
    pub npcs: BTreeMap<u32, Npc>,
    /// Kinds that have ever arrived; dead arrived kinds respawn at dawn
    /// when housing allows (§7.1).
    pub arrived: Vec<NpcKind>,
}

impl Sim {
    // ---- Lifecycle --------------------------------------------------------------

    /// Boot-time roster: the Sage starts at the world spawn (§7.2).
    pub(crate) fn init_npcs(&mut self) {
        let spawn = self.world.spawn;
        let feet = (spawn.0 as f32 + 0.5, (spawn.1 + 1) as f32);
        self.spawn_town_npc(NpcKind::Sage, feet, None);
        self.town.arrived.push(NpcKind::Sage);
    }

    /// Creates the entity + NPC state; returns the entity id.
    fn spawn_town_npc(&mut self, kind: NpcKind, feet: (f32, f32), home: Option<(u32, u32)>) -> u32 {
        let phys = PlayerPhysics::from_feet(feet.0, feet.1);
        let id = self.entities.insert(Entity {
            pos: phys.pos,
            vel: (0.0, 0.0),
            kind: EntityKind::Npc { kind },
            spawn_tick: self.tick,
            awake: true,
            state: npc_anim::FACING_RIGHT,
        });
        self.town.npcs.insert(
            id,
            Npc {
                kind,
                hp: NPC_HP,
                home,
                phys,
                dir: 1,
                pause: 0,
                walk: 0,
            },
        );
        id
    }

    /// Per-tick NPC pipeline: §9 dawn events, hourly arrival checks, AI.
    pub(crate) fn tick_npcs(&mut self) {
        if self.world.time == DAWN_TICK {
            self.npc_dawn();
        } else if self.world.time.is_multiple_of(IN_GAME_HOUR_TICKS) {
            self.npc_arrival_checks();
        }
        self.step_town_npcs();
    }

    /// §9 dawn: housing revalidation, home claims, dead-NPC respawns, and
    /// an arrival check.
    fn npc_dawn(&mut self) {
        let mut changed = false;

        // Revalidate every assigned home (§7.1 "checked every dawn"):
        // rules 1–6 via `check_house`, then rule 7 — players can merge two
        // occupied rooms into one, and the merged room keeps only its first
        // NPC (id order); the rest are evicted to re-claim below.
        let assignments: Vec<(u32, (u32, u32))> = self
            .town
            .npcs
            .iter()
            .filter_map(|(&id, n)| n.home.map(|h| (id, h)))
            .collect();
        let mut kept_homes: Vec<(u32, u32)> = Vec::new();
        for (id, home) in assignments {
            let valid = match check_house(&self.world, home) {
                // Rule 7: an already-kept NPC's home tile inside this room.
                Ok(house) => !kept_homes.iter().any(|&h| house.contains(h)),
                Err(_) => false,
            };
            if valid {
                kept_homes.push(home);
            } else if let Some(n) = self.town.npcs.get_mut(&id) {
                n.home = None;
                changed = true;
            }
        }

        // Homeless living NPCs claim the nearest vacant valid house (§7.1).
        let homeless: Vec<u32> = self
            .town
            .npcs
            .iter()
            .filter(|(_, n)| n.home.is_none())
            .map(|(&id, _)| id)
            .collect();
        for id in homeless {
            let center = self.town_center();
            let occupied = self.occupied_homes();
            if let Some(house) = find_vacant_house(&self.world, center, &occupied) {
                if let Some(n) = self.town.npcs.get_mut(&id) {
                    n.home = Some(house.home);
                    changed = true;
                }
            }
        }

        // Dead arrived kinds respawn into a vacant valid house (§7.1/§9).
        let dead: Vec<NpcKind> = self
            .town
            .arrived
            .iter()
            .copied()
            .filter(|&k| !self.npc_alive(k))
            .collect();
        for kind in dead {
            let center = self.town_center();
            let occupied = self.occupied_homes();
            if let Some(house) = find_vacant_house(&self.world, center, &occupied) {
                let feet = (house.home.0 as f32 + 0.5, (house.home.1 + 1) as f32);
                self.spawn_town_npc(kind, feet, Some(house.home));
                self.broadcast(&ServerMessage::Toast {
                    text: format!("The {} has arrived!", npc_data(kind).kind_name),
                });
                changed = true;
            }
        }

        if self.npc_arrival_checks() {
            changed = false; // arrival already broadcast the roster
        }
        if changed {
            self.broadcast_npc_list();
        }
    }

    /// Evaluates §7.2 arrival conditions for kinds that haven't moved in.
    /// Returns true when somebody arrived (roster already broadcast).
    fn npc_arrival_checks(&mut self) -> bool {
        let mut any = false;
        for kind in NPC_KINDS {
            if self.town.arrived.contains(&kind) {
                continue;
            }
            let condition_met = match npc_data(kind).arrival {
                ArrivalCondition::WorldStart => false, // boot-time only
                // §7.2 "All players": online sessions plus the parked
                // offline players' inventories, summed without double
                // counting (`Sim::combined_player_coins`).
                ArrivalCondition::CombinedCoinsAtLeast(copper) => {
                    self.combined_player_coins() >= copper
                }
                // §7.2 "Any player": offline players' max HP counts too.
                ArrivalCondition::MaxHpOverAndMerchantPresent(hp) => {
                    self.any_player_max_hp_over(hp) && self.npc_alive(NpcKind::Merchant)
                }
            };
            if !condition_met {
                continue;
            }
            let center = self.town_center();
            let occupied = self.occupied_homes();
            let Some(house) = find_vacant_house(&self.world, center, &occupied) else {
                continue; // §7.2: also needs a vacant valid house
            };
            // Spawn at the house's home tile (see module docs).
            let feet = (house.home.0 as f32 + 0.5, (house.home.1 + 1) as f32);
            self.spawn_town_npc(kind, feet, Some(house.home));
            self.town.arrived.push(kind);
            self.broadcast(&ServerMessage::Toast {
                text: format!("The {} has arrived!", npc_data(kind).kind_name),
            });
            any = true;
        }
        if any {
            self.broadcast_npc_list();
        }
        any
    }

    fn npc_alive(&self, kind: NpcKind) -> bool {
        self.town.npcs.values().any(|n| n.kind == kind)
    }

    /// House searches center on the town: the average of housed NPCs'
    /// homes, else the world spawn.
    fn town_center(&self) -> (u32, u32) {
        let homes: Vec<(u32, u32)> = self.occupied_homes();
        if homes.is_empty() {
            return self.world.spawn;
        }
        let (sx, sy) = homes
            .iter()
            .fold((0u64, 0u64), |a, h| (a.0 + h.0 as u64, a.1 + h.1 as u64));
        (
            (sx / homes.len() as u64) as u32,
            (sy / homes.len() as u64) as u32,
        )
    }

    fn occupied_homes(&self) -> Vec<(u32, u32)> {
        self.town.npcs.values().filter_map(|n| n.home).collect()
    }

    // ---- AI -----------------------------------------------------------------------

    /// One tick of NPC behavior (§7.1): wander within 25 tiles of home by
    /// day; stand inside the home at night or while a boss is alive.
    fn step_town_npcs(&mut self) {
        if self.town.npcs.is_empty() {
            return;
        }
        let shelter = !self.world.is_day() || self.any_boss_alive();
        // Disjoint field borrows: world read-only, NPCs + RNG mutable.
        let Sim {
            ref world,
            ref mut town,
            ref mut loot_rng,
            ref mut entities,
            ..
        } = *self;
        for (&id, npc) in town.npcs.iter_mut() {
            step_npc(world, loot_rng, npc, shelter);
            // Mirror into the entity store for snapshots/visibility.
            if let Some(e) = entities.map.get_mut(&id) {
                let state = npc_state_byte(npc);
                let moved = e.pos != npc.phys.pos || e.vel != npc.phys.vel || e.state != state;
                e.pos = npc.phys.pos;
                e.vel = npc.phys.vel;
                e.state = state;
                if moved {
                    e.awake = true;
                }
            }
        }
    }

    /// Whether any boss is currently alive.
    ///
    /// INTEGRATE(boss-alive): no boss entities exist on this branch; the
    /// bosses/enemies branch should make this read the live boss roster.
    /// NPC shelter behavior and the §7.5 `BossAlive` dialogue lines key off
    /// it.
    pub(crate) fn any_boss_alive(&self) -> bool {
        false
    }

    /// Damages a town NPC (enemy contact). Returns the §7.1 fight-back
    /// melee damage (10) the attacker takes in return, or 0 when the target
    /// is gone.
    ///
    /// INTEGRATE(npc-damage): nothing calls this on this branch — the
    /// enemies branch's contact loop is the caller (NPCs take damage *only*
    /// from enemies, §7.1). `attack` is the enemy's raw contact damage; the
    /// §0 formula vs. NPC defense (15) applies here.
    #[allow(dead_code)]
    pub(crate) fn hurt_town_npc(&mut self, id: u32, attack: u32) -> u32 {
        let Some(npc) = self.town.npcs.get_mut(&id) else {
            return 0;
        };
        let dmg = damage_dealt(attack, NPC_DEFENSE);
        npc.hp = npc.hp.saturating_sub(dmg);
        if npc.hp == 0 {
            let kind = npc.kind;
            self.town.npcs.remove(&id);
            self.despawn_entity(id, DespawnReason::Killed);
            self.broadcast(&ServerMessage::Toast {
                text: format!("The {} was slain...", npc_data(kind).kind_name),
            });
            self.broadcast_npc_list();
        }
        NPC_FIGHT_BACK_DAMAGE
    }

    /// Live town-NPC tile positions.
    ///
    /// INTEGRATE(town-suppression): the enemy spawner (§5.3 step 3) consumes
    /// this — each town NPC within 50 tiles of the spawn target scales the
    /// spawn denominator and cap; nothing on this branch calls it.
    #[allow(dead_code)]
    pub fn town_npc_positions(&self) -> Vec<(u32, u32)> {
        self.town
            .npcs
            .values()
            .map(|n| {
                let c = n.center();
                (c.0.max(0.0) as u32, c.1.max(0.0) as u32)
            })
            .collect()
    }

    // ---- Roster sync -----------------------------------------------------------------

    /// Full NPC roster, broadcast on any roster/housing change and sent to
    /// every joining player.
    pub(crate) fn npc_list_message(&self) -> ServerMessage {
        let npcs = self
            .town
            .npcs
            .iter()
            .map(|(&id, n)| NpcInfo {
                id,
                kind: n.kind,
                name: npc_data(n.kind).display_name.to_string(),
                pos: n.phys.pos,
                housed: n.home.is_some(),
            })
            .collect();
        ServerMessage::NpcList { npcs }
    }

    fn broadcast_npc_list(&mut self) {
        let msg = self.npc_list_message();
        self.broadcast(&msg);
    }

    // ---- Interactions (§7.3–§7.5) ------------------------------------------------------

    /// `TalkNpc`: a condition-filtered §7.5 line, plus the shop catalog when
    /// talking to the Merchant.
    pub(crate) fn talk_npc(&mut self, player_id: u32, npc_id: u32) {
        let Some((kind, ctx)) = self.dialogue_ctx(player_id, npc_id) else {
            return;
        };
        let line = pick_line(kind, &ctx, self.loot_rng.next_u32());
        self.send_to(
            player_id,
            &ServerMessage::NpcDialogue {
                npc_id,
                line: line.to_string(),
            },
        );
        if kind == NpcKind::Merchant {
            let items = MERCHANT_SHOP
                .iter()
                .map(|s| ShopEntry {
                    item: s.item,
                    price: s.price,
                })
                .collect();
            self.send_to(player_id, &ServerMessage::ShopContents { npc_id, items });
        }
    }

    /// Builds the §7.5 dialogue context from live state, validating the NPC
    /// and the talk range.
    fn dialogue_ctx(&self, player_id: u32, npc_id: u32) -> Option<(NpcKind, DialogueCtx)> {
        let npc = self.town.npcs.get(&npc_id)?;
        let p = self.players.get(&player_id)?;
        if !in_talk_range(p.center(), npc.center()) {
            return None;
        }
        let flags = self.world.flags;
        let ctx = DialogueCtx {
            night: !self.world.is_day(),
            boss_alive: self.any_boss_alive(),
            watcher_defeated: flags.watcher_defeated,
            slime_monarch_defeated: flags.slime_monarch_defeated,
            bone_warden_defeated: flags.bone_warden_defeated,
            low_hp: p.hp * 100 < p.max_hp * LOW_HP_PERCENT,
            full_hp: p.hp >= p.max_hp,
            rich: coin_total(&p.inventory) > RICH_PLAYER_COPPER,
            potion_sick: p.debuffs.has(Debuff::PotionSickness),
            homeless: npc.home.is_none(),
        };
        Some((npc.kind, ctx))
    }

    /// `BuyItem` (§7.3): validates the merchant, stock, and price, pays in
    /// coins (denominational change-making), and delivers the goods —
    /// inventory first, overflow as world drops.
    pub(crate) fn buy_item(&mut self, player_id: u32, npc_id: u32, item: ItemId, count: u16) {
        if !self.npc_kind_in_reach(player_id, npc_id, NpcKind::Merchant) {
            return;
        }
        let Some(price) = shop_price(item) else {
            tracing::debug!(player = player_id, ?item, "buy of unstocked item");
            return;
        };
        if count == 0 || count > item.max_stack() {
            return;
        }
        let total = price as u64 * count as u64;
        let Some(p) = self.players.get_mut(&player_id) else {
            return;
        };
        let center = p.center();
        let Some((mut changed, change_spill)) = remove_coins(&mut p.inventory, total) else {
            self.send_to(
                player_id,
                &ServerMessage::Toast {
                    text: "Not enough coins.".into(),
                },
            );
            return;
        };
        let (added, add_changed) = add_to_inventory(&mut p.inventory, item, count);
        changed.extend(add_changed);
        self.send_inventory_changes(player_id, changed);
        for spill in change_spill {
            self.spawn_item_drop(spill.item, spill.count, center);
        }
        if added < count {
            // No room: the rest pops out as a world drop at the buyer.
            self.spawn_item_drop(item, count - added, center);
        }
    }

    /// `SellItem` (§7.3): the merchant buys anything back at 20% of value
    /// (rounded down). Zero-value items are unsellable, and so are coins —
    /// `sell_price` is 0 for both, so the refusal below is the
    /// authoritative guard against clients selling currency at 20% of face
    /// value.
    pub(crate) fn sell_item(&mut self, player_id: u32, npc_id: u32, slot: u8, count: u16) {
        if !self.npc_kind_in_reach(player_id, npc_id, NpcKind::Merchant) {
            return;
        }
        let Some(p) = self.players.get_mut(&player_id) else {
            return;
        };
        let idx = slot as usize;
        if idx >= inventory::TOTAL {
            return;
        }
        let Some(stack) = p.inventory.get(idx).copied().flatten() else {
            return;
        };
        if count == 0 || count > stack.count {
            return;
        }
        let each = sell_price(stack.item);
        if each == 0 {
            self.send_to(
                player_id,
                &ServerMessage::Toast {
                    text: format!("The Merchant won't buy {}.", stack.item.data().name),
                },
            );
            return;
        }
        let left = stack.count - count;
        p.inventory[idx] = (left > 0).then_some(InvSlot::new(stack.item, left));
        let total = each as u64 * count as u64;
        let (mut changed, spill) = add_coins(&mut p.inventory, total);
        changed.push(idx);
        let center = p.center();
        self.send_inventory_changes(player_id, changed);
        for s in spill {
            self.spawn_item_drop(s.item, s.count, center);
        }
    }

    /// `NurseHeal` (§7.4): full restore + clears debuffs except Potion
    /// Sickness, for `1 CC × HP + 1 SC × debuff`, ×3/×10 after
    /// Watcher/Bone Warden, minimum 10 CC. Full HP with nothing to clear is
    /// a free no-op.
    pub(crate) fn nurse_heal(&mut self, player_id: u32) {
        let Some(npc_id) = self.npc_id_of_kind(NpcKind::Nurse) else {
            return;
        };
        if !self.npc_kind_in_reach(player_id, npc_id, NpcKind::Nurse) {
            return;
        }
        let flags = self.world.flags;
        let Some(p) = self.players.get_mut(&player_id) else {
            return;
        };
        let hp_restored = p.max_hp.saturating_sub(p.hp);
        if hp_restored == 0 {
            // §7.4: no effect and no charge at full HP.
            self.send_to(
                player_id,
                &ServerMessage::Toast {
                    text: "You're already at full health.".into(),
                },
            );
            return;
        }
        let cleared = p.debuffs.nurse_clearable();
        let cost = nurse_heal_cost(
            hp_restored,
            cleared,
            flags.watcher_defeated,
            flags.bone_warden_defeated,
        );
        let center = p.center();
        let Some((changed, spill)) = remove_coins(&mut p.inventory, cost) else {
            self.send_to(
                player_id,
                &ServerMessage::Toast {
                    text: "Not enough coins.".into(),
                },
            );
            return;
        };
        p.hp = p.max_hp;
        p.debuffs.clear_except_potion_sickness();
        let health = ServerMessage::PlayerHealth {
            id: player_id,
            hp: p.hp as u16,
            max_hp: p.max_hp as u16,
        };
        let debuffs = ServerMessage::PlayerDebuffs {
            id: player_id,
            debuffs: p.debuffs.as_wire(),
        };
        self.send_inventory_changes(player_id, changed);
        for s in spill {
            self.spawn_item_drop(s.item, s.count, center);
        }
        self.broadcast(&health);
        self.broadcast(&debuffs);
    }

    /// `SetBedSpawn` (§8): right-clicking a bed within reach stores the
    /// per-player spawn at the bed's origin.
    pub(crate) fn set_bed_spawn(&mut self, player_id: u32, x: u32, y: u32) {
        if !self.world.in_bounds(x, y) || self.world.tile(x, y).id != TileId::Bed {
            return;
        }
        let origin = self.world.multitile_origin(x, y);
        let Some(p) = self.players.get_mut(&player_id) else {
            return;
        };
        if !bed_in_reach(p.center(), origin) {
            return;
        }
        p.bed_spawn = Some(origin);
        self.send_to(
            player_id,
            &ServerMessage::Toast {
                text: "Spawn point set!".into(),
            },
        );
    }

    /// Where `player_id` respawns (§8): standing on their bed if the bed
    /// still exists (checked now, at respawn time), else the world spawn.
    /// Returns the AABB top-left like all player positions.
    ///
    /// INTEGRATE(bed-respawn): the respawn flow (enemies/combat branch —
    /// death, timers, `ClientMessage::Respawn`) places the player with
    /// this; nothing on this branch dies.
    #[allow(dead_code)]
    pub(crate) fn spawn_point_for(&self, player_id: u32) -> (f32, f32) {
        let world_spawn = PlayerPhysics::from_feet(
            self.world.spawn.0 as f32 + 0.5,
            (self.world.spawn.1 + 1) as f32,
        )
        .pos;
        let Some(p) = self.players.get(&player_id) else {
            return world_spawn;
        };
        let Some(origin) = p.bed_spawn else {
            return world_spawn;
        };
        // Bed destroyed since it was set → world-spawn fallback.
        if self.world.tile(origin.0, origin.1).id != TileId::Bed
            || self.world.multitile_origin(origin.0, origin.1) != origin
        {
            return world_spawn;
        }
        let (w, h) = TileId::Bed.data().size;
        // Feet on the floor the bed stands on, centered on the bed.
        PlayerPhysics::from_feet(
            origin.0 as f32 + w as f32 / 2.0,
            (origin.1 + h as u32) as f32,
        )
        .pos
    }

    // ---- Plumbing -------------------------------------------------------------------

    /// The live NPC entity id of `kind`, if any.
    pub(crate) fn npc_id_of_kind(&self, kind: NpcKind) -> Option<u32> {
        self.town
            .npcs
            .iter()
            .find(|(_, n)| n.kind == kind)
            .map(|(&id, _)| id)
    }

    /// Shared validation: `npc_id` exists, is `kind`, and the player is in
    /// talk range.
    fn npc_kind_in_reach(&self, player_id: u32, npc_id: u32, kind: NpcKind) -> bool {
        let Some(npc) = self.town.npcs.get(&npc_id) else {
            return false;
        };
        let Some(p) = self.players.get(&player_id) else {
            return false;
        };
        npc.kind == kind && in_talk_range(p.center(), npc.center())
    }

    /// Sends `SlotChanged` for every touched slot and refreshes the held
    /// item broadcast if the held stack was among them.
    fn send_inventory_changes(&mut self, player_id: u32, mut changed: Vec<usize>) {
        changed.sort_unstable();
        changed.dedup();
        let Some(p) = self.players.get(&player_id) else {
            return;
        };
        let held_changed = changed.contains(&(p.held_slot as usize));
        let deltas: Vec<(u8, Option<InvSlot>)> = changed
            .into_iter()
            .filter(|&i| i < p.inventory.len())
            .map(|i| (i as u8, p.inventory[i]))
            .collect();
        for (idx, stack) in deltas {
            self.send_to(player_id, &ServerMessage::SlotChanged { idx, stack });
        }
        if held_changed {
            self.broadcast_held_item(player_id);
        }
    }
}

fn npc_state_byte(npc: &Npc) -> u8 {
    let mut s = 0;
    if npc.dir > 0 {
        s |= npc_anim::FACING_RIGHT;
    }
    if npc.phys.vel.0.abs() > 0.3 {
        s |= npc_anim::WALKING;
    }
    s
}

/// Player-to-NPC interaction range test (center to center).
fn in_talk_range(player_center: (f32, f32), npc_center: (f32, f32)) -> bool {
    let (dx, dy) = (
        player_center.0 - npc_center.0,
        player_center.1 - npc_center.1,
    );
    dx * dx + dy * dy <= NPC_TALK_RANGE * NPC_TALK_RANGE
}

/// Bed reach: any cell of the 4×2 footprint within §8 reach of the player.
fn bed_in_reach(center: (f32, f32), origin: (u32, u32)) -> bool {
    let (w, h) = TileId::Bed.data().size;
    for dy in 0..h as u32 {
        for dx in 0..w as u32 {
            if ferraria_shared::tile_in_reach(center, origin.0 + dx, origin.1 + dy) {
                return true;
            }
        }
    }
    false
}

/// One tick of a single NPC: shelter (stand at home) at night/boss, §7.1
/// leashed wander by day. Pure over the world, so it's unit-testable.
fn step_npc(world: &World, rng: &mut Pcg32, npc: &mut Npc, shelter: bool) {
    let mut input = PlayerInput::default();
    if shelter {
        if let Some(home) = npc.home {
            // Stand inside the home: teleport there if away (see module
            // docs — no pathfinding in v1).
            let at_home = (npc.center().0 - (home.0 as f32 + 0.5)).abs() <= 1.5
                && (npc.phys.feet_y() - (home.1 as f32 + 1.0)).abs() <= 2.0;
            if !at_home {
                npc.phys = PlayerPhysics::from_feet(home.0 as f32 + 0.5, (home.1 + 1) as f32);
            }
        }
        npc.walk = 0;
    } else if npc.pause > 0 {
        npc.pause -= 1;
    } else {
        if npc.walk == 0 {
            // Re-roll behavior: usually walk a stretch, sometimes idle.
            if rng.chance(0.35) {
                npc.pause = rng.gen_range_u32(WANDER_PAUSE_TICKS.0..WANDER_PAUSE_TICKS.1);
            } else {
                npc.walk = rng.gen_range_u32(WANDER_WALK_TICKS.0..WANDER_WALK_TICKS.1);
                npc.dir = if rng.chance(0.5) { 1 } else { -1 };
            }
        }
        if npc.walk > 0 {
            npc.walk -= 1;
            // §7.1 leash: within 25 tiles of home (world spawn if homeless).
            let anchor = npc.home.unwrap_or(world.spawn);
            let cx = npc.center().0;
            if cx > anchor.0 as f32 + NPC_WANDER_RADIUS {
                npc.dir = -1;
            } else if cx < anchor.0 as f32 - NPC_WANDER_RADIUS {
                npc.dir = 1;
            }
            // Don't stroll off cliffs deeper than a few tiles.
            if npc.phys.on_ground && drop_ahead(world, npc) > LEDGE_MAX_DROP {
                npc.dir = -npc.dir;
            }
            input.left = npc.dir < 0;
            input.right = npc.dir > 0;
            // Closed door directly ahead: teleport through (module docs).
            // Other 2-high obstacles: hop (1-high auto-steps).
            if let Some(blocker) = front_blocker(world, npc) {
                if world.tile(blocker.0, blocker.1).id == TileId::Door {
                    teleport_through_door(world, npc, blocker.0);
                } else {
                    input.jump = npc.phys.on_ground;
                }
            }
        }
    }
    step_player_with_mods(
        world,
        &mut npc.phys,
        input,
        DT,
        PhysicsMods {
            speed_mult: NPC_WALK_SPEED_MULT,
            ..PhysicsMods::NONE
        },
    );
}

/// How many tiles of drop are under the cell just ahead of the NPC's feet
/// (capped at `LEDGE_MAX_DROP + 1`).
fn drop_ahead(world: &World, npc: &Npc) -> u32 {
    let ahead_x = (npc.center().0 + npc.dir as f32 * (PLAYER_WIDTH / 2.0 + 0.6)).floor() as i32;
    let feet_row = npc.phys.feet_y().floor() as i32;
    for d in 0..=LEDGE_MAX_DROP {
        let y = feet_row + d as i32;
        if world.is_solid(ahead_x, y) || world.is_platform(ahead_x, y) {
            return d;
        }
    }
    LEDGE_MAX_DROP + 1
}

/// The solid tile blocking the NPC's path at body height, if any.
fn front_blocker(world: &World, npc: &Npc) -> Option<(u32, u32)> {
    let ahead_x = if npc.dir > 0 {
        npc.phys.pos.0 + PLAYER_WIDTH + 0.2
    } else {
        npc.phys.pos.0 - 0.2
    };
    if ahead_x < 0.0 {
        return None;
    }
    let x = ahead_x.floor() as u32;
    // Probe above the 1-tile auto-step: mid-body height.
    let y = (npc.phys.pos.1 + PLAYER_HEIGHT - 1.5).floor();
    if y < 0.0 {
        return None;
    }
    let y = y as u32;
    (world.in_bounds(x, y) && world.tile(x, y).is_solid()).then_some((x, y))
}

/// Door pass-through (module docs): hop the NPC to the far side of the door
/// column when the destination is clear, as if it opened and closed the
/// door behind itself.
fn teleport_through_door(world: &World, npc: &mut Npc, door_x: u32) {
    let dest_x = if npc.dir > 0 {
        door_x as f32 + 1.0 + 0.05
    } else {
        door_x as f32 - PLAYER_WIDTH - 0.05
    };
    if dest_x < 0.0 {
        return;
    }
    let feet = npc.phys.feet_y();
    // Destination column must be standable: body clear, floor below.
    let col = (dest_x + PLAYER_WIDTH / 2.0).floor() as i32;
    let feet_row = (feet - 0.1).floor() as i32;
    let clear = (0..3).all(|d| !world.is_solid(col, feet_row - d));
    let floored = world.is_solid(col, feet_row + 1) || world.is_platform(col, feet_row + 1);
    if clear && floored {
        npc.phys.pos.0 = dest_x;
        npc.phys.vel.0 = 0.0;
    } else {
        // Jammed doorway: turn around.
        npc.dir = -npc.dir;
    }
}

#[cfg(test)]
mod tests {
    use super::super::game::Sim;
    use super::super::test_util::*;
    use super::*;
    use ferraria_shared::coins::coin_value;
    use ferraria_shared::npc::{MERCHANT_ARRIVAL_COPPER, NURSE_MIN_COST};
    use ferraria_shared::protocol::ClientMessage;
    use ferraria_shared::tiles::{state, WallId};
    use ferraria_shared::world::DAY_TICKS;

    const FLOOR: u32 = 30;

    fn coins_of(sim: &Sim, id: u32) -> u64 {
        coin_total(&sim.players[&id].inventory)
    }

    /// Builds a §7.1-valid house with its interior's bottom-left at
    /// (x0, floor-1) into the sim's world (10×4 interior).
    fn build_house(sim: &mut Sim, x0: u32, floor: u32) {
        let ih = 4u32;
        let y_top = floor - ih;
        for x in x0 - 1..=x0 + 10 {
            for y in y_top - 1..=floor {
                let interior = (x0..x0 + 10).contains(&x) && (y_top..floor).contains(&y);
                let mut t = sim.world().tile(x, y);
                if interior {
                    t.id = TileId::Air;
                    t.wall = WallId::Wood;
                } else {
                    t.id = TileId::Stone;
                }
                t.state = 0;
                sim.change_tile(x, y, t);
            }
        }
        for d in 0..3u32 {
            let mut t = sim.world().tile(x0 - 1, floor - 1 - d);
            t.id = TileId::Door;
            t.state = state::part(0, (2 - d) as u8);
            sim.change_tile(x0 - 1, floor - 1 - d, t);
        }
        let mut torch = sim.world().tile(x0, floor - 1);
        torch.id = TileId::Torch;
        sim.change_tile(x0, floor - 1, torch);
        assert!(sim.world.place_multitile(x0 + 1, floor - 2, TileId::Table));
        assert!(sim.world.place_multitile(x0 + 4, floor - 2, TileId::Chair));
    }

    /// Advances the sim to the next `time % IN_GAME_HOUR_TICKS == 0`
    /// boundary (the arrival check).
    fn advance_to_hour(sim: &mut Sim) {
        let next = IN_GAME_HOUR_TICKS - (sim.world.time % IN_GAME_HOUR_TICKS);
        advance(sim, next);
    }

    fn advance_to_dawn(sim: &mut Sim) {
        let next = (DAWN_TICK + DAY_TICKS - sim.world.time) % DAY_TICKS;
        advance(sim, if next == 0 { DAY_TICKS } else { next });
    }

    #[test]
    fn sage_spawns_at_world_spawn_on_boot() {
        let sim = flat_sim(300, 60, FLOOR);
        assert_eq!(sim.town.npcs.len(), 1);
        let sage = sim.town.npcs.values().next().expect("sage");
        assert_eq!(sage.kind, NpcKind::Sage);
        assert!(sage.home.is_none(), "no house exists yet");
        assert!((sage.center().0 - 150.5).abs() < 2.0);
        // The roster reaches joining players.
        let mut sim = sim;
        let (_id, _e, mut rx) = join(&mut sim, "alice");
        let npcs = drain(&mut rx)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::NpcList { npcs } => Some(npcs),
                _ => None,
            })
            .expect("NpcList in join state");
        assert_eq!(npcs.len(), 1);
        assert_eq!(npcs[0].kind, NpcKind::Sage);
        assert_eq!(npcs[0].name, "Sage the Guide");
        assert!(!npcs[0].housed);
    }

    #[test]
    fn merchant_arrives_when_combined_coins_and_a_house_exist() {
        let mut sim = flat_sim(300, 60, FLOOR);
        build_house(&mut sim, 160, FLOOR);
        let (a, _ea, mut rx_a) = join(&mut sim, "alice");
        let (b, _eb, _rx_b) = join(&mut sim, "bob");
        drain(&mut rx_a);

        // 30 SC on alice + 19 SC 99 CC on bob: 1 copper short of 50 SC.
        give(&mut sim, a, 5, ItemId::SilverCoin, 30);
        give(&mut sim, b, 5, ItemId::SilverCoin, 19);
        give(&mut sim, b, 6, ItemId::CopperCoin, 99);
        assert_eq!(coins_of(&sim, a) + coins_of(&sim, b), 4_999);
        advance_to_hour(&mut sim);
        assert!(!sim.npc_alive(NpcKind::Merchant), "one copper short");

        // Top up to exactly 50 SC combined (denominations mixed).
        give(&mut sim, b, 7, ItemId::CopperCoin, 1);
        assert_eq!(
            coins_of(&sim, a) + coins_of(&sim, b),
            MERCHANT_ARRIVAL_COPPER
        );
        advance_to_hour(&mut sim);
        assert!(sim.npc_alive(NpcKind::Merchant), "merchant moved in");
        let merchant_id = sim.npc_id_of_kind(NpcKind::Merchant).expect("id");
        assert!(
            sim.town.npcs[&merchant_id].home.is_some(),
            "claimed the house"
        );
        let msgs = drain(&mut rx_a);
        assert!(
            msgs.iter().any(
                |m| matches!(m, ServerMessage::Toast { text } if text == "The Merchant has arrived!")
            ),
            "arrival toast"
        );
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::NpcList { npcs } if npcs.iter().any(|n| n.kind == NpcKind::Merchant))));
    }

    #[test]
    fn merchant_needs_a_vacant_house() {
        let mut sim = flat_sim(300, 60, FLOOR);
        let (a, _ea, _rx) = join(&mut sim, "alice");
        give(&mut sim, a, 5, ItemId::GoldCoin, 1); // plenty of money
        advance_to_hour(&mut sim);
        assert!(!sim.npc_alive(NpcKind::Merchant), "no house, no merchant");
    }

    #[test]
    fn nurse_needs_life_crystal_hp_and_the_merchant() {
        let mut sim = flat_sim(300, 60, FLOOR);
        build_house(&mut sim, 160, FLOOR);
        build_house(&mut sim, 180, FLOOR);
        let (a, _ea, _rx) = join(&mut sim, "alice");
        give(&mut sim, a, 5, ItemId::GoldCoin, 1);

        // Max HP 100: even with the merchant condition pending, no nurse.
        advance_to_hour(&mut sim);
        assert!(sim.npc_alive(NpcKind::Merchant));
        assert!(!sim.npc_alive(NpcKind::Nurse), "max HP not over 100");

        sim.players.get_mut(&a).expect("p").max_hp = 120;
        advance_to_hour(&mut sim);
        assert!(sim.npc_alive(NpcKind::Nurse), "nurse moved in");
    }

    #[test]
    fn offline_players_count_toward_arrival_conditions() {
        use super::super::game::SimCommand;
        let mut sim = flat_sim(300, 60, FLOOR);
        build_house(&mut sim, 160, FLOOR);
        build_house(&mut sim, 180, FLOOR);
        let (a, _ea, _rx_a) = join(&mut sim, "alice");
        let (b, eb, _rx_b) = join(&mut sim, "bob");
        // Bob holds most of the wealth and the Life Crystal HP, then logs
        // off: §7.2 says "All players" / "Any player", not "online".
        give(&mut sim, a, 5, ItemId::SilverCoin, 10);
        give(&mut sim, b, 5, ItemId::SilverCoin, 40);
        sim.players.get_mut(&b).expect("p").max_hp = 120;
        sim.handle(SimCommand::Disconnect {
            player_id: b,
            epoch: eb,
        });
        assert!(!sim.players.contains_key(&b), "bob parked offline");
        assert_eq!(coins_of(&sim, a), 1_000, "only 10 SC remain online");

        advance_to_hour(&mut sim);
        assert!(
            sim.npc_alive(NpcKind::Merchant),
            "parked coins counted toward 50 SC"
        );
        assert!(sim.npc_alive(NpcKind::Nurse), "parked max HP > 100 counted");
    }

    #[test]
    fn dead_npcs_respawn_at_dawn_only_with_valid_housing() {
        let mut sim = flat_sim(300, 60, FLOOR);
        let sage = sim.npc_id_of_kind(NpcKind::Sage).expect("sage");
        // Beat the homeless Sage to death through the damage seam.
        let mut fight_back = 0;
        for _ in 0..50 {
            fight_back = sim.hurt_town_npc(sage, 100);
            if !sim.town.npcs.contains_key(&sage) {
                break;
            }
        }
        assert_eq!(fight_back, NPC_FIGHT_BACK_DAMAGE, "npc fought back");
        assert!(!sim.npc_alive(NpcKind::Sage), "slain");
        assert!(!sim.entities.map.contains_key(&sage), "entity despawned");

        // Dawn with no housing: still dead.
        advance_to_dawn(&mut sim);
        assert!(!sim.npc_alive(NpcKind::Sage));

        // Build a house; next dawn he returns, housed.
        build_house(&mut sim, 160, FLOOR);
        advance_to_dawn(&mut sim);
        assert!(sim.npc_alive(NpcKind::Sage), "respawned at dawn");
        let id = sim.npc_id_of_kind(NpcKind::Sage).expect("id");
        assert!(sim.town.npcs[&id].home.is_some());
    }

    #[test]
    fn hp_math_respects_npc_defense() {
        let mut sim = flat_sim(300, 60, FLOOR);
        let sage = sim.npc_id_of_kind(NpcKind::Sage).expect("sage");
        // §0 formula vs 15 defense: 20 attack → 13 damage.
        sim.hurt_town_npc(sage, 20);
        assert_eq!(sim.town.npcs[&sage].hp, NPC_HP - 13);
    }

    #[test]
    fn homeless_npc_claims_nearest_house_at_dawn_and_loses_it_when_broken() {
        let mut sim = flat_sim(300, 60, FLOOR);
        build_house(&mut sim, 160, FLOOR);
        advance_to_dawn(&mut sim);
        let sage = sim.npc_id_of_kind(NpcKind::Sage).expect("sage");
        let home = sim.town.npcs[&sage].home.expect("claimed at dawn");

        // Knock the torch out: rule 4 fails at the next dawn revalidation.
        let mut t = sim.world().tile(160, FLOOR - 1);
        assert_eq!(t.id, TileId::Torch);
        t.id = TileId::Air;
        sim.change_tile(160, FLOOR - 1, t);
        advance_to_dawn(&mut sim);
        let npc_home = sim.town.npcs[&sage].home;
        assert_ne!(npc_home, Some(home), "stale claim revalidated away");
        assert_eq!(npc_home, None);
    }

    #[test]
    fn dawn_evicts_cohabitants_when_occupied_rooms_merge() {
        let mut sim = flat_sim(300, 60, FLOOR);
        build_house(&mut sim, 160, FLOOR);
        build_house(&mut sim, 172, FLOOR);
        advance_to_dawn(&mut sim);
        let sage = sim.npc_id_of_kind(NpcKind::Sage).expect("sage");
        let sage_home = sim.town.npcs[&sage].home.expect("sage housed");
        // House a Merchant in the other room directly (arrival conditions
        // are exercised elsewhere).
        let other =
            find_vacant_house(&sim.world, sim.world.spawn, &[sage_home]).expect("second house");
        assert_ne!(other.home, sage_home);
        let feet = (other.home.0 as f32 + 0.5, (other.home.1 + 1) as f32);
        sim.spawn_town_npc(NpcKind::Merchant, feet, Some(other.home));

        // Knock out the dividing walls (and the second door): the two
        // occupied rooms become one room, still §7.1-valid by rules 1–6.
        for x in [170u32, 171] {
            for dy in 1..=4u32 {
                let mut t = sim.world().tile(x, FLOOR - dy);
                t.id = TileId::Air;
                t.state = 0;
                sim.change_tile(x, FLOOR - dy, t);
            }
        }
        let merged = check_house(&sim.world, sage_home).expect("merged room valid");
        assert!(merged.contains(other.home), "one room holds both homes");

        // Dawn revalidation applies rule 7: exactly one NPC keeps a home
        // (the other finds no vacant room — the merged one is taken).
        advance_to_dawn(&mut sim);
        let housed: Vec<NpcKind> = sim
            .town
            .npcs
            .values()
            .filter(|n| n.home.is_some())
            .map(|n| n.kind)
            .collect();
        assert_eq!(housed, vec![NpcKind::Sage], "first by id keeps the room");
    }

    #[test]
    fn talk_npc_returns_a_dialogue_line_in_range_only() {
        let mut sim = flat_sim(300, 60, FLOOR);
        let (a, ea, mut rx) = join(&mut sim, "alice");
        let sage = sim.npc_id_of_kind(NpcKind::Sage).expect("sage");
        drain(&mut rx);

        // In range (player joins at spawn, sage stands there too).
        msg(&mut sim, a, ea, ClientMessage::TalkNpc { npc_id: sage });
        let line = drain(&mut rx)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::NpcDialogue { npc_id, line } if npc_id == sage => Some(line),
                _ => None,
            })
            .expect("dialogue line");
        // Daytime, default-state, homeless Sage: the three §7.5 defaults
        // plus the homeless line.
        let expected = ferraria_shared::npc::eligible_lines(
            NpcKind::Sage,
            &DialogueCtx {
                homeless: true,
                full_hp: true,
                ..DialogueCtx::default()
            },
        );
        assert!(expected.contains(&line.as_str()), "{line}");

        // Walk far away: talking yields nothing.
        place_player(&mut sim, a, 50.0, FLOOR as f32);
        msg(&mut sim, a, ea, ClientMessage::TalkNpc { npc_id: sage });
        assert!(drain(&mut rx)
            .iter()
            .all(|m| !matches!(m, ServerMessage::NpcDialogue { .. })));
    }

    #[test]
    fn dialogue_context_reads_live_player_state() {
        let mut sim = flat_sim(300, 60, FLOOR);
        let (a, _ea, _rx) = join(&mut sim, "alice");
        let sage = sim.npc_id_of_kind(NpcKind::Sage).expect("sage");

        let (_, ctx) = sim.dialogue_ctx(a, sage).expect("ctx");
        assert!(!ctx.night);
        assert!(ctx.homeless, "sage starts homeless");
        assert!(ctx.full_hp);
        assert!(!ctx.low_hp);
        assert!(!ctx.rich);
        assert!(!ctx.potion_sick);

        // Hurt + enrich + sicken the player, push night.
        {
            let p = sim.players.get_mut(&a).expect("p");
            p.hp = 20; // 20% of 100
            p.inventory[9] = Some(InvSlot::new(ItemId::GoldCoin, 2));
            p.debuffs
                .insert_for_test(Debuff::PotionSickness, 60 * ferraria_shared::TICK_RATE);
        }
        sim.world.time = 0; // midnight
        sim.world.flags.watcher_defeated = true;
        let (_, ctx) = sim.dialogue_ctx(a, sage).expect("ctx");
        assert!(ctx.night);
        assert!(ctx.low_hp);
        assert!(!ctx.full_hp);
        assert!(ctx.rich);
        assert!(ctx.potion_sick);
        assert!(ctx.watcher_defeated);
    }

    /// Sets up a sim with a housed merchant next to the player.
    fn merchant_sim() -> (
        Sim,
        u32,
        u64,
        tokio::sync::mpsc::Receiver<super::super::game::Frame>,
        u32,
    ) {
        let mut sim = flat_sim(300, 60, FLOOR);
        build_house(&mut sim, 145, FLOOR);
        let (a, ea, mut rx) = join(&mut sim, "alice");
        give(&mut sim, a, 5, ItemId::SilverCoin, 50);
        advance_to_hour(&mut sim);
        let merchant = sim.npc_id_of_kind(NpcKind::Merchant).expect("arrived");
        // Stand next to the merchant.
        let mpos = sim.town.npcs[&merchant].center();
        place_player(&mut sim, a, mpos.0 + 2.0, FLOOR as f32);
        drain(&mut rx);
        (sim, a, ea, rx, merchant)
    }

    #[test]
    fn talk_to_merchant_includes_the_shop_catalog() {
        let (mut sim, a, ea, mut rx, merchant) = merchant_sim();
        msg(&mut sim, a, ea, ClientMessage::TalkNpc { npc_id: merchant });
        let items = drain(&mut rx)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::ShopContents { npc_id, items } if npc_id == merchant => Some(items),
                _ => None,
            })
            .expect("shop contents");
        assert_eq!(items.len(), MERCHANT_SHOP.len());
        assert!(items
            .iter()
            .any(|e| e.item == ItemId::Torch && e.price == 50));
        assert!(items
            .iter()
            .any(|e| e.item == ItemId::MiningHelmet && e.price == 40_000));
    }

    #[test]
    fn buy_item_charges_coins_and_delivers() {
        let (mut sim, a, ea, mut rx, merchant) = merchant_sim();
        // 3 Lesser Healing Potions at 3 SC each = 9 SC.
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::BuyItem {
                npc_id: merchant,
                item: ItemId::LesserHealingPotion,
                count: 3,
            },
        );
        assert_eq!(coins_of(&sim, a), 4_100);
        let potions: u32 = sim.players[&a]
            .inventory
            .iter()
            .flatten()
            .filter(|s| s.item == ItemId::LesserHealingPotion)
            .map(|s| s.count as u32)
            .sum();
        assert_eq!(potions, 3);
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::SlotChanged { stack: Some(s), .. }
                if s.item == ItemId::LesserHealingPotion && s.count == 3)));

        // Unstocked item: refused silently, nothing charged.
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::BuyItem {
                npc_id: merchant,
                item: ItemId::GoldBar,
                count: 1,
            },
        );
        assert_eq!(coins_of(&sim, a), 4_100);

        // Too expensive: 4 GC helmet vs 41 SC wallet.
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::BuyItem {
                npc_id: merchant,
                item: ItemId::MiningHelmet,
                count: 1,
            },
        );
        assert_eq!(coins_of(&sim, a), 4_100, "not charged");
        assert!(drain(&mut rx).iter().any(
            |m| matches!(m, ServerMessage::Toast { text } if text.contains("Not enough coins"))
        ));
    }

    #[test]
    fn buy_overflow_drops_at_the_player() {
        let (mut sim, a, ea, _rx, merchant) = merchant_sim();
        // Fill every carry slot except the coin stack: the purchase can't
        // fit and spills as a drop.
        {
            let p = sim.players.get_mut(&a).expect("p");
            for i in 0..inventory::ARMOR_START {
                if p.inventory[i].is_none_or(|s| s.item != ItemId::SilverCoin) {
                    p.inventory[i] = Some(InvSlot::new(ItemId::Stone, 999));
                }
            }
        }
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::BuyItem {
                npc_id: merchant,
                item: ItemId::Torch,
                count: 5,
            },
        );
        // 5 torches = 250 CC, paid by breaking 3 SC. The 50 CC change has
        // no slot either, so it spills as a drop alongside the torches.
        assert_eq!(coins_of(&sim, a), 5_000 - 300);
        let dropped = |item: ItemId| -> u32 {
            sim.entities
                .map
                .values()
                .filter_map(|e| match e.kind {
                    EntityKind::ItemDrop { item: i, count } if i == item => Some(count as u32),
                    _ => None,
                })
                .sum()
        };
        assert_eq!(dropped(ItemId::Torch), 5, "torches spilled as a drop");
        assert_eq!(dropped(ItemId::CopperCoin), 50, "change spilled too");
    }

    #[test]
    fn sell_item_pays_20_percent_rounded_down() {
        let (mut sim, a, ea, mut rx, merchant) = merchant_sim();
        let start = coins_of(&sim, a);
        give(&mut sim, a, 6, ItemId::Wood, 30); // value 10 → sells at 2
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SellItem {
                npc_id: merchant,
                slot: 6,
                count: 10,
            },
        );
        assert_eq!(coins_of(&sim, a), start + 20);
        assert_eq!(
            sim.players[&a].inventory[6],
            Some(InvSlot::new(ItemId::Wood, 20))
        );
        drain(&mut rx);

        // Zero-value items are unsellable.
        give(&mut sim, a, 7, ItemId::Dirt, 50);
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SellItem {
                npc_id: merchant,
                slot: 7,
                count: 50,
            },
        );
        assert_eq!(
            sim.players[&a].inventory[7],
            Some(InvSlot::new(ItemId::Dirt, 50)),
            "dirt not consumed"
        );
        assert_eq!(coins_of(&sim, a), start + 20);
        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Toast { text } if text.contains("won't buy"))));

        // Over-count is rejected.
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SellItem {
                npc_id: merchant,
                slot: 6,
                count: 21,
            },
        );
        assert_eq!(
            sim.players[&a].inventory[6],
            Some(InvSlot::new(ItemId::Wood, 20))
        );
    }

    #[test]
    fn coins_cannot_be_sold_to_the_merchant() {
        let (mut sim, a, ea, mut rx, merchant) = merchant_sim();
        let start = coins_of(&sim, a);
        // §7.3 "buys back any item" doesn't cover currency: 10 SC would
        // fetch 200 CC and destroy the other 800. The server refuses no
        // matter what gesture the client routed here.
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SellItem {
                npc_id: merchant,
                slot: 5,
                count: 10,
            },
        );
        assert_eq!(coins_of(&sim, a), start, "wallet untouched");
        assert_eq!(
            sim.players[&a].inventory[5],
            Some(InvSlot::new(ItemId::SilverCoin, 50)),
            "coin stack untouched"
        );
        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::Toast { text } if text.contains("won't buy"))));
    }

    /// Housed merchant + nurse next to the player. The receiver must stay
    /// alive (a closed outbound channel kicks the session).
    #[allow(clippy::type_complexity)]
    fn nurse_sim() -> (
        Sim,
        u32,
        u64,
        tokio::sync::mpsc::Receiver<super::super::game::Frame>,
        u32,
    ) {
        let mut sim = flat_sim(300, 60, FLOOR);
        build_house(&mut sim, 145, FLOOR);
        build_house(&mut sim, 165, FLOOR);
        let (a, ea, rx) = join(&mut sim, "alice");
        give(&mut sim, a, 5, ItemId::SilverCoin, 50);
        sim.players.get_mut(&a).expect("p").max_hp = 200;
        advance_to_hour(&mut sim); // merchant + nurse conditions both hold
        let nurse = sim.npc_id_of_kind(NpcKind::Nurse).expect("nurse arrived");
        let npos = sim.town.npcs[&nurse].center();
        place_player(&mut sim, a, npos.0 + 2.0, FLOOR as f32);
        (sim, a, ea, rx, nurse)
    }

    #[test]
    fn nurse_heal_costs_hp_plus_debuffs_with_boss_multipliers() {
        let (mut sim, a, ea, _rx, _nurse) = nurse_sim();
        {
            let p = sim.players.get_mut(&a).expect("p");
            p.hp = 50; // restore 150
            p.debuffs.insert_for_test(Debuff::Burning, 100);
            p.debuffs.insert_for_test(Debuff::PotionSickness, 100);
        }
        let start = coins_of(&sim, a);
        msg(&mut sim, a, ea, ClientMessage::NurseHeal);
        let p = &sim.players[&a];
        assert_eq!(p.hp, 200, "healed to full");
        // 150 CC + 1 SC (one clearable debuff — potion sickness exempt).
        assert_eq!(coins_of(&sim, a), start - 250);
        assert!(!p.debuffs.has(Debuff::Burning), "burning cleared");
        assert!(
            p.debuffs.has(Debuff::PotionSickness),
            "potion sickness stays"
        );

        // ×3 after the Watcher.
        sim.world.flags.watcher_defeated = true;
        sim.players.get_mut(&a).expect("p").hp = 100; // restore 100
        let start = coins_of(&sim, a);
        msg(&mut sim, a, ea, ClientMessage::NurseHeal);
        assert_eq!(coins_of(&sim, a), start - 300);

        // ×10 after the Bone Warden; minimum 10 CC.
        sim.world.flags.bone_warden_defeated = true;
        sim.players.get_mut(&a).expect("p").hp = 199; // restore 1 → 10 CC min
        let start = coins_of(&sim, a);
        msg(&mut sim, a, ea, ClientMessage::NurseHeal);
        assert_eq!(coins_of(&sim, a), start - NURSE_MIN_COST);
    }

    #[test]
    fn nurse_full_hp_is_a_free_no_op_and_poverty_refuses() {
        let (mut sim, a, ea, _rx, _nurse) = nurse_sim();
        sim.players.get_mut(&a).expect("p").hp = 200; // at the raised max
        let start = coins_of(&sim, a);
        msg(&mut sim, a, ea, ClientMessage::NurseHeal);
        assert_eq!(coins_of(&sim, a), start, "full HP: no charge");

        // Broke and hurt: refused, HP untouched.
        {
            let p = sim.players.get_mut(&a).expect("p");
            p.inventory[5] = None; // drop the coins
            p.hp = 50;
        }
        msg(&mut sim, a, ea, ClientMessage::NurseHeal);
        assert_eq!(sim.players[&a].hp, 50, "no heal without payment");
    }

    #[test]
    fn bed_spawn_set_validate_and_fallback() {
        let mut sim = flat_sim(300, 60, FLOOR);
        let (a, ea, mut rx) = join(&mut sim, "alice");
        let world_spawn = sim.spawn_point_for(a);

        // No bed set: world spawn.
        assert_eq!(world_spawn, sim.spawn_point_for(a));

        // A bed near the player.
        place_player(&mut sim, a, 150.0, FLOOR as f32);
        assert!(sim.world.place_multitile(152, FLOOR - 2, TileId::Bed));
        drain(&mut rx);
        // Click a non-origin cell of the bed: stored at the origin.
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SetBedSpawn {
                x: 154,
                y: FLOOR - 1,
            },
        );
        assert_eq!(sim.players[&a].bed_spawn, Some((152, FLOOR - 2)));
        assert!(drain(&mut rx).iter().any(
            |m| matches!(m, ServerMessage::Toast { text } if text.contains("Spawn point set"))
        ));
        let bed_spawn = sim.spawn_point_for(a);
        assert_ne!(bed_spawn, world_spawn);
        // Standing centered on the bed footprint.
        assert!((bed_spawn.0 + PLAYER_WIDTH / 2.0 - 154.0).abs() < 0.6);

        // Out-of-reach bed: rejected.
        assert!(sim.world.place_multitile(170, FLOOR - 2, TileId::Bed));
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SetBedSpawn {
                x: 170,
                y: FLOOR - 2,
            },
        );
        assert_eq!(sim.players[&a].bed_spawn, Some((152, FLOOR - 2)));

        // Not a bed: rejected.
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SetBedSpawn { x: 150, y: FLOOR },
        );
        assert_eq!(sim.players[&a].bed_spawn, Some((152, FLOOR - 2)));

        // Destroying the bed falls back to world spawn at respawn time.
        sim.break_tile(152, FLOOR - 2);
        assert_eq!(sim.spawn_point_for(a), world_spawn);
    }

    #[test]
    fn npcs_wander_by_day_but_stay_leashed_and_shelter_at_night() {
        let mut sim = flat_sim(300, 60, FLOOR);
        build_house(&mut sim, 160, FLOOR);
        advance_to_dawn(&mut sim);
        let sage = sim.npc_id_of_kind(NpcKind::Sage).expect("sage");
        let home = sim.town.npcs[&sage].home.expect("housed");

        // The dawn claim re-anchored the leash from the world spawn (where
        // the homeless Sage wandered up to 25 tiles out) to the new home:
        // it may legally start outside the new leash, so let it stroll back
        // inside before holding it to the bound.
        for _ in 0..6000 {
            if (sim.town.npcs[&sage].center().0 - home.0 as f32).abs() <= NPC_WANDER_RADIUS {
                break;
            }
            sim.tick();
        }

        // A day of wandering: never strays past the §7.1 leash (+ slack
        // for the in-flight stretch when the leash flips the direction).
        for _ in 0..2000 {
            sim.tick();
            let c = sim.town.npcs[&sage].center();
            assert!(
                (c.0 - home.0 as f32).abs() <= NPC_WANDER_RADIUS + 3.0,
                "leash held: {c:?} vs home {home:?}"
            );
        }

        // Push to night: the NPC stands at home.
        sim.world.time = ferraria_shared::world::DUSK_TICK;
        advance(&mut sim, 5);
        let c = sim.town.npcs[&sage].center();
        assert!(
            (c.0 - (home.0 as f32 + 0.5)).abs() <= 1.6,
            "standing at home: {c:?} vs {home:?}"
        );
        // And the entity mirror followed.
        let e = &sim.entities.map[&sage];
        assert_eq!(e.pos, sim.town.npcs[&sage].phys.pos);
    }

    #[test]
    fn town_npc_positions_reports_live_tiles() {
        let sim = flat_sim(300, 60, FLOOR);
        let positions = sim.town_npc_positions();
        assert_eq!(positions.len(), 1);
        assert!((positions[0].0 as i64 - 150).abs() <= 1);
    }

    #[test]
    fn join_receives_health_and_roster() {
        let mut sim = flat_sim(300, 60, FLOOR);
        let (id, _e, mut rx) = join(&mut sim, "alice");
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::PlayerHealth { id: pid, hp: 100, max_hp: 100 } if *pid == id)));
        assert!(msgs
            .iter()
            .any(|m| matches!(m, ServerMessage::NpcList { .. })));
    }

    #[test]
    fn reclaim_preserves_hp_and_bed_spawn() {
        use super::super::game::SimCommand;
        let mut sim = flat_sim(300, 60, FLOOR);
        let (a, ea, mut rx) = join(&mut sim, "alice");
        let msgs = drain(&mut rx);
        let token = msgs
            .iter()
            .find_map(|m| match m {
                ServerMessage::Welcome { token, .. } => Some(*token),
                _ => None,
            })
            .expect("token");
        place_player(&mut sim, a, 150.0, FLOOR as f32);
        assert!(sim.world.place_multitile(152, FLOOR - 2, TileId::Bed));
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SetBedSpawn {
                x: 152,
                y: FLOOR - 2,
            },
        );
        sim.players.get_mut(&a).expect("p").max_hp = 140;
        sim.players.get_mut(&a).expect("p").hp = 77;
        sim.handle(SimCommand::Disconnect {
            player_id: a,
            epoch: ea,
        });
        let (tx, _rx2) = tokio::sync::mpsc::channel(super::super::game::OUTBOUND_QUEUE_FRAMES);
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        sim.handle(SimCommand::Join {
            name: "alice".into(),
            token: Some(token),
            tx,
            reply: reply_tx,
        });
        let (re_id, _) = reply_rx.try_recv().expect("reply").expect("accepted");
        assert_eq!(re_id, a);
        let p = &sim.players[&a];
        assert_eq!((p.hp, p.max_hp), (77, 140));
        assert_eq!(p.bed_spawn, Some((152, FLOOR - 2)));
    }

    #[test]
    fn sheltered_npcs_still_broadcast_snapshots() {
        let (mut sim, _a, _ea, mut rx, merchant) = merchant_sim();
        // A night-sheltered NPC moves nothing, so `step_town_npcs` leaves
        // its entity asleep — but snapshots must keep flowing (NPCs get no
        // spawn-message re-sync on chunk re-subscribe; a stale mirror makes
        // talk/heal/shop intents fail the server range check against the
        // real position).
        sim.entities
            .map
            .get_mut(&merchant)
            .expect("merchant entity")
            .awake = false;
        sim.broadcast_entity_updates();
        let seen = drain(&mut rx).into_iter().any(|m| {
            matches!(
                m,
                ServerMessage::EntityUpdate { entities }
                    if entities.iter().any(|e| e.id == merchant)
            )
        });
        assert!(seen, "NPC snapshots are exempt from the awake gate");
    }

    #[test]
    fn sell_change_uses_canonical_denominations() {
        let (mut sim, a, ea, _rx, merchant) = merchant_sim();
        // Sell a Gold Bar (1200 → 240 CC): paid as 2 SC + 40 CC.
        give(&mut sim, a, 6, ItemId::GoldBar, 1);
        let before = coins_of(&sim, a);
        msg(
            &mut sim,
            a,
            ea,
            ClientMessage::SellItem {
                npc_id: merchant,
                slot: 6,
                count: 1,
            },
        );
        assert_eq!(coins_of(&sim, a), before + 240);
        assert_eq!(sim.players[&a].inventory[6], None);
        let coppers: u32 = sim.players[&a]
            .inventory
            .iter()
            .flatten()
            .filter(|s| s.item == ItemId::CopperCoin)
            .map(|s| s.count as u32)
            .sum();
        assert_eq!(coppers * coin_value(ItemId::CopperCoin), 40);
    }
}
