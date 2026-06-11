//! Player survival (§8): server-authoritative HP, breath/drowning, fall
//! damage, lava/Hellstone contact, the timed-debuff engine, passive regen,
//! death (coin drop + respawn timer) and respawn, healing items, and the
//! bed spawn point.
//!
//! ## Fall damage with client-authoritative movement
//!
//! The client owns its own movement (ARCHITECTURE.md), so the server can't
//! run the §8 fall physics itself. Instead it derives the fall from the
//! stream of accepted `PlayerState`s: downward displacement accumulates
//! while the reported state is airborne, resets when the reported body
//! center is submerged (the same test the shared physics uses to zero its
//! own fall distance), and the §8 formula applies on the airborne→grounded
//! transition. Gust Jar negation can't be observed (a mid-air jump is just
//! more client motion), so the client raises [`anim::AIR_JUMPED`] in its
//! next state; the server honors the flag only when a Gust Jar is actually
//! equipped. A client lying about the flag without the accessory gains
//! nothing, and one lying *with* it equipped only negates what the
//! accessory legitimately negates.

use ferraria_shared::items::{inventory, InvSlot, ItemId, POTION_SICKNESS_SECS};
use ferraria_shared::loadout;
use ferraria_shared::physics::{fall_damage, hitbox, PlayerPhysics, PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::{anim, ActiveDebuff, Debuff, ServerMessage};
use ferraria_shared::tiles::{LiquidKind, TileId, LAVA_BURN_SECS, LAVA_CONTACT_DAMAGE};
use ferraria_shared::world::World;
use ferraria_shared::{
    damage_dealt, BREATH_DRAIN_INTERVAL_TICKS, BREATH_REFILL_PER_TICK, BURNING_DPS,
    DEATH_COIN_DROP_FRAC, DROWNING_DPS, PLAYER_BASE_MAX_HP, PLAYER_IFRAME_TICKS, PLAYER_MAX_BREATH,
    PLAYER_MAX_MAX_HP, REGEN_DELAY_SECS, REGEN_HP_PER_SEC, REGEN_STANDING_STILL_MULT, RESPAWN_SECS,
    RESPAWN_SECS_BOSS_ALIVE, TICK_RATE,
};

use super::game::Sim;

/// How player damage interacts with defense and §0 i-frames.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Hurt {
    /// A hit: defense applies, gated by (and triggering) the §0 0.67 s
    /// player i-frames, optionally shoving via `PlayerKnockback`.
    Hit { knockback: Option<(f32, f32)> },
    /// Damage-over-time / environment (drowning, Burning, fall): ignores
    /// defense and bypasses i-frames without triggering them.
    Raw,
}

/// Breath refill messages are throttled to one per this many ticks (drain
/// already paces itself at 1 per 7 ticks).
const BREATH_SYNC_TICKS: u64 = 6;

impl Sim {
    // ---- Damage / death ------------------------------------------------------

    /// Damages a player. Returns whether damage was actually applied
    /// (false when dead or inside the i-frame window).
    pub(crate) fn hurt_player(&mut self, id: u32, attack: u32, how: Hurt) -> bool {
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return false;
        };
        if p.dead {
            return false;
        }
        let damage = match how {
            Hurt::Hit { .. } => {
                if tick < p.iframe_until {
                    return false;
                }
                p.iframe_until = tick + PLAYER_IFRAME_TICKS as u64;
                damage_dealt(attack, loadout::defense(&p.inventory))
            }
            Hurt::Raw => attack,
        };
        if damage == 0 {
            return false;
        }
        p.hp = p.hp.saturating_sub(damage.min(u16::MAX as u32) as u16);
        p.last_damage_tick = tick;
        p.regen_acc = 0.0;
        let hp = p.hp;
        if let Hurt::Hit {
            knockback: Some((vx, vy)),
        } = how
        {
            self.send_to(id, &ServerMessage::PlayerKnockback { vx, vy });
        }
        self.sync_health(id);
        if hp == 0 {
            self.kill_player(id);
        }
        true
    }

    /// Broadcasts a player's current HP (everyone renders it; the owner
    /// drives the hearts row).
    pub(crate) fn sync_health(&mut self, id: u32) {
        let Some(p) = self.players.get(&id) else {
            return;
        };
        self.broadcast(&ServerMessage::PlayerHealth {
            id,
            hp: p.hp,
            max_hp: p.max_hp,
        });
    }

    /// §8 death: drop 50% of carried coins where they died (a normal
    /// item-drop pile — those persist 10 min), clear debuffs, start the
    /// respawn timer (10 s; 20 s while a boss is alive).
    fn kill_player(&mut self, id: u32) {
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        p.dead = true;
        p.hp = 0;
        p.swing = None;
        p.debuffs.clear();
        let center = p.center();
        let name = p.name.clone();
        // INTEGRATE(boss-alive): the boss branch tracks live bosses; the
        // §8 respawn timer doubles while one is alive.
        let boss_alive = false;
        let secs = if boss_alive {
            RESPAWN_SECS_BOSS_ALIVE
        } else {
            RESPAWN_SECS
        };
        p.respawn_ready_tick = tick + (secs * TICK_RATE) as u64;

        // 50% of each carried coin stack (hotbar + backpack), rounded down.
        let mut dropped: Vec<(ItemId, u16)> = Vec::new();
        let mut deltas: Vec<(u8, Option<InvSlot>)> = Vec::new();
        for i in 0..inventory::ARMOR_START {
            let Some(stack) = p.inventory[i] else {
                continue;
            };
            if !matches!(
                stack.item,
                ItemId::CopperCoin | ItemId::SilverCoin | ItemId::GoldCoin | ItemId::PlatinumCoin
            ) {
                continue;
            }
            let drop = (stack.count as f32 * DEATH_COIN_DROP_FRAC) as u16;
            if drop == 0 {
                continue;
            }
            let left = stack.count - drop;
            p.inventory[i] = (left > 0).then_some(InvSlot::new(stack.item, left));
            deltas.push((i as u8, p.inventory[i]));
            dropped.push((stack.item, drop));
        }
        for (idx, stack) in deltas {
            self.send_to(id, &ServerMessage::SlotChanged { idx, stack });
        }
        for (item, count) in dropped {
            self.spawn_item_drop(item, count, center);
        }
        self.send_debuffs(id);
        self.broadcast(&ServerMessage::PlayerDied { id });
        tracing::info!(player = id, name = %name, "player died");
    }

    /// `Respawn` intent: allowed once the §8 timer elapsed; respawns at the
    /// bed spawn (if its bed still stands) or the world spawn with
    /// `max(100, maxHP/2)` HP.
    pub(crate) fn respawn_player(&mut self, id: u32) {
        let tick = self.tick;
        let world_spawn = spawn_pos(&self.world);
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if !p.dead || tick < p.respawn_ready_tick {
            return;
        }
        let pos = p
            .bed_spawn
            .and_then(|origin| bed_spawn_pos(&self.world, origin))
            .unwrap_or(world_spawn);
        p.dead = false;
        p.hp = (PLAYER_BASE_MAX_HP.max(p.max_hp as u32 / 2) as u16).min(p.max_hp);
        p.breath = PLAYER_MAX_BREATH as u16;
        p.iframe_until = tick + PLAYER_IFRAME_TICKS as u64;
        p.fall_accum = 0.0;
        p.was_grounded = true;
        p.pos = pos;
        p.vel = (0.0, 0.0);
        p.moved = true;
        let facing = p.facing;
        self.broadcast(&ServerMessage::PlayerRespawned { id, pos });
        // Authoritative own-position correction: the client snaps its
        // prediction to the respawn point (same mechanism as join reclaim).
        self.send_to(
            id,
            &ServerMessage::PlayerMoved {
                id,
                pos,
                vel: (0.0, 0.0),
                facing,
                anim: 0,
            },
        );
        self.sync_health(id);
        self.update_player_chunks(id);
    }

    /// `SetBedSpawn` (§2 tile 27): right-clicking a bed in reach sets the
    /// personal spawn to that bed's origin.
    pub(crate) fn set_bed_spawn(&mut self, id: u32, x: u32, y: u32) {
        if !self.world.in_bounds(x, y) || self.world.tile(x, y).id != TileId::Bed {
            return;
        }
        let origin = self.world.multitile_origin(x, y);
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if !ferraria_shared::tile_in_reach(p.center(), x, y) {
            return;
        }
        p.bed_spawn = Some(origin);
        self.send_to(
            id,
            &ServerMessage::Toast {
                text: "Spawn point set".into(),
            },
        );
    }

    // ---- Debuffs ---------------------------------------------------------------

    /// Adds (or refreshes — durations don't stack) a timed debuff and syncs.
    pub(crate) fn add_debuff(&mut self, id: u32, debuff: Debuff, ticks: u32) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if p.dead {
            return;
        }
        // Burning immunity (Obsidian Charm / Ember set, §4.3) blocks the
        // debuff outright.
        if debuff == Debuff::Burning && loadout::effect_mods(&p.inventory).burn_immune {
            return;
        }
        if let Some(d) = p.debuffs.iter_mut().find(|d| d.0 == debuff) {
            d.1 = d.1.max(ticks);
        } else {
            p.debuffs.push((debuff, ticks));
        }
        self.send_debuffs(id);
    }

    pub(crate) fn has_debuff(&self, id: u32, debuff: Debuff) -> bool {
        self.players
            .get(&id)
            .is_some_and(|p| p.debuffs.iter().any(|d| d.0 == debuff))
    }

    /// Broadcasts the full replacement debuff list for `id` (clients count
    /// the remaining ticks down locally between syncs).
    fn send_debuffs(&mut self, id: u32) {
        let Some(p) = self.players.get(&id) else {
            return;
        };
        let debuffs: Vec<ActiveDebuff> = p
            .debuffs
            .iter()
            .map(|&(debuff, remaining_ticks)| ActiveDebuff {
                debuff,
                remaining_ticks,
            })
            .collect();
        self.broadcast(&ServerMessage::PlayerDebuffs { id, debuffs });
    }

    // ---- Healing items (§4.4) ---------------------------------------------------

    /// Drinks a healing potion from `slot` (blocked by Potion Sickness).
    pub(crate) fn drink_healing(&mut self, id: u32, slot: u8, heal: u16) {
        if self.has_debuff(id, Debuff::PotionSickness) {
            return;
        }
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if p.dead || p.hp >= p.max_hp {
            return;
        }
        p.hp = (p.hp + heal).min(p.max_hp);
        self.consume_from_slot(id, slot);
        self.sync_health(id);
        self.add_debuff(
            id,
            Debuff::PotionSickness,
            (POTION_SICKNESS_SECS * TICK_RATE as f32) as u32,
        );
    }

    /// Life Crystal (§8): +20 max HP up to 400, healing the same amount.
    pub(crate) fn use_life_crystal(&mut self, id: u32, slot: u8, add: u16) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if p.dead || p.max_hp as u32 >= PLAYER_MAX_MAX_HP {
            return;
        }
        p.max_hp = (p.max_hp + add).min(PLAYER_MAX_MAX_HP as u16);
        p.hp = (p.hp + add).min(p.max_hp);
        self.consume_from_slot(id, slot);
        self.sync_health(id);
    }

    /// Removes one item from a hotbar slot with the owner's delta.
    fn consume_from_slot(&mut self, id: u32, slot: u8) {
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

    // ---- Movement observation (fall damage, §8) ----------------------------------

    /// Called for every accepted `PlayerState` (see the module docs for the
    /// design): accumulates observed falls and applies the §8 fall-damage
    /// formula on landing.
    pub(crate) fn observe_movement(&mut self, id: u32, old_pos: (f32, f32), new_anim: u8) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if p.dead {
            p.fall_accum = 0.0;
            return;
        }
        let grounded = new_anim & anim::GROUNDED != 0;
        let mods = loadout::physics_mods(&p.inventory);
        let submerged = body_center_submerged(&self.world, p.pos);

        if new_anim & anim::AIR_JUMPED != 0 && mods.extra_air_jumps > 0 {
            // Gust Jar mid-air jump negates the fall so far (§8).
            p.fall_accum = 0.0;
        }
        if submerged {
            // §8: landing in deep liquid is safe — and the shared physics
            // zeroes its own fall counter while swimming, so mirror that.
            p.fall_accum = 0.0;
        } else if !(grounded && p.was_grounded) {
            // Any downward motion not between two grounded states is fall:
            // this includes the takeoff and landing transition states, so a
            // ledge drop spanning one report isn't undercounted. Walking
            // down slopes (grounded→grounded) never accumulates.
            p.fall_accum += (p.pos.1 - old_pos.1).max(0.0);
        }

        let landed = grounded && !p.was_grounded;
        p.was_grounded = grounded;
        if landed {
            let fall = p.fall_accum;
            p.fall_accum = 0.0;
            if !mods.no_fall_damage {
                let dmg = fall_damage(fall);
                if dmg > 0 {
                    // Raw: the §8 formula is the final damage (not combat,
                    // so the §0 defense formula doesn't apply).
                    self.hurt_player(id, dmg, Hurt::Raw);
                }
            }
        }
    }

    // ---- Per-tick vitals -----------------------------------------------------------

    /// Breath, drowning, lava/Hellstone contact, debuff timers + Burning
    /// DPS, and passive regen — every tick, every living player.
    pub(crate) fn tick_player_vitals(&mut self) {
        let ids: Vec<u32> = self.players.keys().copied().collect();
        for id in ids {
            self.tick_breath(id);
            self.tick_contact_hazards(id);
            self.tick_debuffs(id);
            self.tick_regen(id);
        }
    }

    /// §8 breath: 200 units; −1 per 7 ticks while the head is submerged,
    /// +3/tick out of liquid; at 0, 10 dmg/s ignoring defense.
    fn tick_breath(&mut self, id: u32) {
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if p.dead {
            return;
        }
        let head_under = head_submerged(&self.world, p.pos);
        let mut changed = false;
        if head_under {
            if tick.is_multiple_of(BREATH_DRAIN_INTERVAL_TICKS as u64) {
                if p.breath > 0 {
                    p.breath -= 1;
                    changed = true;
                } else {
                    // Drowning: 10/s, ignoring defense, no i-frames.
                    p.drown_acc +=
                        DROWNING_DPS as f32 * BREATH_DRAIN_INTERVAL_TICKS as f32 / TICK_RATE as f32;
                    let whole = p.drown_acc.floor() as u32;
                    p.drown_acc -= whole as f32;
                    if whole > 0 {
                        self.hurt_player(id, whole, Hurt::Raw);
                    }
                    return;
                }
            }
        } else if p.breath < PLAYER_MAX_BREATH as u16 {
            p.breath = (p.breath + BREATH_REFILL_PER_TICK as u16).min(PLAYER_MAX_BREATH as u16);
            p.drown_acc = 0.0;
            changed =
                tick.is_multiple_of(BREATH_SYNC_TICKS) || p.breath == PLAYER_MAX_BREATH as u16;
        }
        if changed {
            let breath = p.breath;
            self.send_to(id, &ServerMessage::PlayerBreath { id, breath });
        }
    }

    /// Lava contact (§3: 50 damage + Burning 7 s, halved by the Obsidian
    /// Charm) and Hellstone contact (§2: Burning 2 s unless fire-immune).
    fn tick_contact_hazards(&mut self, id: u32) {
        let Some(p) = self.players.get(&id) else {
            return;
        };
        if p.dead {
            return;
        }
        let pos = p.pos;
        let effects = loadout::effect_mods(&p.inventory);
        if touches_lava(&self.world, pos) {
            let attack = (LAVA_CONTACT_DAMAGE as f32 * effects.lava_damage_mult) as u32;
            if self.hurt_player(id, attack, Hurt::Hit { knockback: None }) {
                self.add_debuff(
                    id,
                    Debuff::Burning,
                    (LAVA_BURN_SECS * TICK_RATE as f32) as u32,
                );
            }
        }
        if !effects.burn_immune && touches_hellstone(&self.world, pos) {
            // §2 tile 11: Burning 2 s on touch.
            self.add_debuff(id, Debuff::Burning, 2 * TICK_RATE);
        }
    }

    /// Ticks debuff timers down (syncing on expiry) and applies Burning's
    /// 2 dmg/s (ignores defense).
    fn tick_debuffs(&mut self, id: u32) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if p.dead || p.debuffs.is_empty() {
            return;
        }
        let burning = p.debuffs.iter().any(|d| d.0 == Debuff::Burning);
        for d in p.debuffs.iter_mut() {
            d.1 = d.1.saturating_sub(1);
        }
        let expired = p.debuffs.iter().any(|d| d.1 == 0);
        p.debuffs.retain(|d| d.1 > 0);
        if burning {
            p.burn_acc += BURNING_DPS as f32 / TICK_RATE as f32;
            let whole = p.burn_acc.floor() as u32;
            p.burn_acc -= whole as f32;
            if whole > 0 {
                self.hurt_player(id, whole, Hurt::Raw);
            }
        }
        if expired {
            self.send_debuffs(id);
        }
    }

    /// §8 passive regen: 0.5 HP/s once 8 s passed without damage, ×2 while
    /// standing still, +0.5 with the Band of Vigor.
    fn tick_regen(&mut self, id: u32) {
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if p.dead || p.hp == 0 || p.hp >= p.max_hp {
            return;
        }
        let delay = (REGEN_DELAY_SECS * TICK_RATE as f32) as u64;
        if tick.saturating_sub(p.last_damage_tick) < delay {
            return;
        }
        let standing_still =
            p.vel.0.abs() < 0.05 && p.vel.1.abs() < 0.05 && p.anim & anim::GROUNDED != 0;
        let mut rate = REGEN_HP_PER_SEC;
        if standing_still {
            rate *= REGEN_STANDING_STILL_MULT;
        }
        rate += loadout::effect_mods(&p.inventory).regen_bonus_hps;
        p.regen_acc += rate / TICK_RATE as f32;
        if p.regen_acc >= 1.0 {
            let whole = p.regen_acc.floor() as u16;
            p.regen_acc -= whole as f32;
            p.hp = (p.hp + whole).min(p.max_hp);
            self.sync_health(id);
        }
    }
}

// ---- Pure world tests ------------------------------------------------------------

pub(crate) use super::game::spawn_pos;

/// Feet on top of a bed whose origin (top-left of the 4×2) is `origin`.
/// `None` when the bed no longer exists (§8: fall back to world spawn).
pub(crate) fn bed_spawn_pos(world: &World, origin: (u32, u32)) -> Option<(f32, f32)> {
    if world.tile(origin.0, origin.1).id != TileId::Bed {
        return None;
    }
    let (w, _) = TileId::Bed.data().size;
    Some(PlayerPhysics::from_feet(origin.0 as f32 + w as f32 / 2.0, origin.1 as f32).pos)
}

/// Liquid covers a world point: the cell holds liquid and its fill level
/// (which rises from the cell floor) reaches up past the point.
fn point_in_liquid(world: &World, x: f32, y: f32) -> Option<LiquidKind> {
    let liquid = world.liquid(x.floor() as i32, y.floor() as i32);
    let kind = liquid.kind()?;
    let cell_bottom = y.floor() + 1.0;
    let surface =
        cell_bottom - liquid.level() as f32 / ferraria_shared::tiles::LIQUID_MAX_LEVEL as f32;
    (y >= surface).then_some(kind)
}

/// §8 "fully submerged": the head point (near the top of the AABB) is under
/// liquid.
fn head_submerged(world: &World, pos: (f32, f32)) -> bool {
    point_in_liquid(world, pos.0 + PLAYER_WIDTH / 2.0, pos.1 + 0.3).is_some()
}

/// Body center submerged — the same test the shared physics' swim mode
/// uses; drives fall-negation on landing in deep liquid.
fn body_center_submerged(world: &World, pos: (f32, f32)) -> bool {
    point_in_liquid(
        world,
        pos.0 + PLAYER_WIDTH / 2.0,
        pos.1 + PLAYER_HEIGHT / 2.0,
    )
    .is_some()
}

/// Any cell of the player AABB holds lava.
fn touches_lava(world: &World, pos: (f32, f32)) -> bool {
    let (w, h) = hitbox::PLAYER;
    let (x0, y0) = (pos.0.floor() as i32, pos.1.floor() as i32);
    let (x1, y1) = ((pos.0 + w).floor() as i32, (pos.1 + h).floor() as i32);
    for y in y0..=y1 {
        for x in x0..=x1 {
            if world.liquid(x, y).kind() == Some(LiquidKind::Lava) {
                return true;
            }
        }
    }
    false
}

/// The player touches Hellstone: any AABB-overlapped cell, or the row the
/// feet rest on, is Hellstone (§2 tile 11).
fn touches_hellstone(world: &World, pos: (f32, f32)) -> bool {
    let (w, h) = hitbox::PLAYER;
    let (x0, y0) = (pos.0.floor() as i32, pos.1.floor() as i32);
    let (x1, y1) = ((pos.0 + w).floor() as i32, (pos.1 + h + 0.1).floor() as i32);
    for y in y0..=y1 {
        for x in x0..=x1 {
            if x >= 0 && y >= 0 && world.tile(x as u32, y as u32).id == TileId::Hellstone {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::entities::EntityKind;
    use super::super::game::Sim;
    use super::super::test_util::*;
    use super::*;
    use ferraria_shared::protocol::ClientMessage;
    use ferraria_shared::tiles::{Liquid, Tile};
    use ferraria_shared::RESPAWN_SECS;

    const FLOOR: u32 = 70;

    fn setup() -> (
        Sim,
        u32,
        u64,
        tokio::sync::mpsc::Receiver<super::super::game::Frame>,
    ) {
        let mut sim = flat_sim(100, 100, FLOOR);
        let (id, epoch, mut rx) = join(&mut sim, "vita");
        drain(&mut rx);
        place_player(&mut sim, id, 50.0, FLOOR as f32);
        (sim, id, epoch, rx)
    }

    /// A walled basin of still water deep enough to submerge the player.
    fn build_pool(sim: &mut Sim, x0: u32, x1: u32, depth: u32) {
        for y in (FLOOR - depth)..FLOOR {
            for x in x0..=x1 {
                let mut t = Tile::AIR;
                t.liquid = Liquid::new(LiquidKind::Water, 8);
                sim.change_tile(x, y, t);
            }
            // Stone walls so the §3 automaton can't drain it sideways.
            sim.change_tile(x0 - 1, y, Tile::of(TileId::Stone));
            sim.change_tile(x1 + 1, y, Tile::of(TileId::Stone));
        }
    }

    /// Sends an accepted PlayerState placing the feet-center at (x, y_feet).
    fn report_state(sim: &mut Sim, id: u32, epoch: u64, x: f32, y_feet: f32, anim: u8) {
        msg(
            sim,
            id,
            epoch,
            ClientMessage::PlayerState {
                pos: (x - PLAYER_WIDTH / 2.0, y_feet - PLAYER_HEIGHT - 1e-3),
                vel: (0.0, 0.0),
                facing: 1,
                anim,
            },
        );
    }

    // ---- Breath / drowning (§8) -------------------------------------------------

    #[test]
    fn breath_drains_drowns_and_refills_on_the_design_timeline() {
        let (mut sim, id, _epoch, mut rx) = setup();
        build_pool(&mut sim, 40, 60, 8);
        place_player(&mut sim, id, 50.0, FLOOR as f32); // fully submerged
        drain(&mut rx);

        // Drain: 1 unit per 7 ticks.
        advance(&mut sim, 70);
        let b = sim.players[&id].breath;
        assert!((189..=191).contains(&b), "70 ticks ≈ 10 units ({b})");
        assert!(drain(&mut rx)
            .iter()
            .any(|m| matches!(m, ServerMessage::PlayerBreath { .. })));

        // Empty the meter; drowning damage at 10/s ignoring defense.
        sim.players.get_mut(&id).expect("p").breath = 0;
        let hp0 = sim.players[&id].hp;
        advance(&mut sim, 6 * 60);
        let hp = sim.players[&id].hp;
        let lost = (hp0 - hp) as i32;
        assert!(
            (55..=62).contains(&lost),
            "≈10 dmg/s while drowning ({lost})"
        );

        // Climb out: refills 3/tick (≈1.1 s to full).
        place_player(&mut sim, id, 20.0, FLOOR as f32);
        advance(&mut sim, 70);
        assert_eq!(sim.players[&id].breath, PLAYER_MAX_BREATH as u16);
    }

    // ---- Fall damage (§8) ----------------------------------------------------------

    /// Walks the player through a reported fall of `tiles`, returns HP lost.
    fn run_fall(sim: &mut Sim, id: u32, epoch: u64, tiles: f32, air_jump_at: Option<f32>) -> u32 {
        let hp0 = sim.players[&id].hp;
        let x = 50.0;
        let top = FLOOR as f32 - tiles;
        report_state(sim, id, epoch, x, top, anim::GROUNDED);
        advance(sim, 1);
        let mut y = top;
        while y < FLOOR as f32 {
            y = (y + 5.0).min(FLOOR as f32);
            let mut flags = 0u8;
            if let Some(at) = air_jump_at {
                if y >= at && y - 5.0 < at {
                    flags |= anim::AIR_JUMPED;
                }
            }
            report_state(sim, id, epoch, x, y, flags);
            advance(sim, 1);
        }
        report_state(sim, id, epoch, x, FLOOR as f32, anim::GROUNDED);
        advance(sim, 1);
        (hp0 - sim.players[&id].hp) as u32
    }

    #[test]
    fn fall_damage_threshold_at_25_tiles() {
        let (mut sim, id, epoch, _rx) = setup();
        assert_eq!(run_fall(&mut sim, id, epoch, 25.0, None), 0, "25 is safe");
        assert_eq!(
            run_fall(&mut sim, id, epoch, 26.0, None),
            10,
            "10 dmg per tile past 25"
        );
        sim.players.get_mut(&id).expect("p").hp = 100;
        assert_eq!(run_fall(&mut sim, id, epoch, 30.0, None), 50);
    }

    #[test]
    fn lucky_charm_negates_fall_damage() {
        let (mut sim, id, epoch, _rx) = setup();
        let p = sim.players.get_mut(&id).expect("p");
        p.inventory[inventory::ACCESSORY_START] = Some(InvSlot::new(ItemId::LuckyCharm, 1));
        assert_eq!(run_fall(&mut sim, id, epoch, 40.0, None), 0);
    }

    #[test]
    fn gust_jar_air_jump_resets_the_fall() {
        let (mut sim, id, epoch, _rx) = setup();
        {
            let p = sim.players.get_mut(&id).expect("p");
            p.inventory[inventory::ACCESSORY_START] = Some(InvSlot::new(ItemId::GustJar, 1));
        }
        // 40-tile fall with a mid-air jump 10 tiles above ground: only the
        // last 10 tiles count → safe.
        let dmg = run_fall(&mut sim, id, epoch, 40.0, Some(FLOOR as f32 - 10.0));
        assert_eq!(dmg, 0, "air jump negates the fall so far");
        // Without the Gust Jar equipped, the same flag is ignored. (Enough
        // max HP that the 150 fall damage doesn't clip at death.)
        {
            let p = sim.players.get_mut(&id).expect("p");
            p.inventory[inventory::ACCESSORY_START] = None;
            p.max_hp = 400;
            p.hp = 400;
        }
        let dmg = run_fall(&mut sim, id, epoch, 40.0, Some(FLOOR as f32 - 10.0));
        assert_eq!(dmg, 150, "lying about AIR_JUMPED without the jar is futile");
    }

    #[test]
    fn deep_water_landing_negates_fall_damage() {
        let (mut sim, id, epoch, _rx) = setup();
        build_pool(&mut sim, 45, 55, 6);
        let dmg = run_fall(&mut sim, id, epoch, 40.0, None);
        assert_eq!(dmg, 0, "§8: landing in deep liquid is safe");
    }

    // ---- Death & respawn (§8) ---------------------------------------------------

    #[test]
    fn death_drops_half_coins_and_respawn_restores_design_hp() {
        let (mut sim, id, epoch, mut rx) = setup();
        {
            let p = sim.players.get_mut(&id).expect("p");
            p.inventory[5] = Some(InvSlot::new(ItemId::CopperCoin, 75));
            p.inventory[6] = Some(InvSlot::new(ItemId::SilverCoin, 3));
        }
        drain(&mut rx);
        assert!(sim.hurt_player(id, 1000, Hurt::Raw));
        assert!(sim.players[&id].dead);
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::PlayerDied { id: pid } if *pid == id)));
        // 50% of each coin stack (rounded down) dropped at the corpse.
        let p = &sim.players[&id];
        assert_eq!(p.inventory[5], Some(InvSlot::new(ItemId::CopperCoin, 38)));
        assert_eq!(p.inventory[6], Some(InvSlot::new(ItemId::SilverCoin, 2)));
        let dropped: Vec<(ItemId, u16)> = sim
            .entities
            .map
            .values()
            .filter_map(|e| match e.kind {
                EntityKind::ItemDrop { item, count } => Some((item, count)),
                _ => None,
            })
            .collect();
        assert!(dropped.contains(&(ItemId::CopperCoin, 37)));
        assert!(dropped.contains(&(ItemId::SilverCoin, 1)));

        // Too early: the §8 timer (10 s) gates the respawn.
        msg(&mut sim, id, epoch, ClientMessage::Respawn);
        assert!(sim.players[&id].dead, "timer not elapsed");
        advance(&mut sim, RESPAWN_SECS * 60 + 1);
        msg(&mut sim, id, epoch, ClientMessage::Respawn);
        let p = &sim.players[&id];
        assert!(!p.dead);
        assert_eq!(p.hp, 100, "max(100, 100/2) = 100");
        let world_spawn = spawn_pos(sim.world());
        assert!((p.pos.0 - world_spawn.0).abs() < 0.01, "at world spawn");
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::PlayerRespawned { id: pid, .. } if *pid == id)));
        assert!(
            msgs.iter().any(|m| matches!(m,
                ServerMessage::PlayerMoved { id: pid, .. } if *pid == id)),
            "own-position correction so the client snaps"
        );
    }

    #[test]
    fn respawn_hp_is_max_of_100_and_half_max() {
        let (mut sim, id, epoch, _rx) = setup();
        {
            let p = sim.players.get_mut(&id).expect("p");
            p.max_hp = 400;
            p.hp = 400;
        }
        sim.hurt_player(id, 5000, Hurt::Raw);
        advance(&mut sim, RESPAWN_SECS * 60 + 1);
        msg(&mut sim, id, epoch, ClientMessage::Respawn);
        assert_eq!(sim.players[&id].hp, 200, "max(100, 400/2)");
    }

    #[test]
    fn bed_spawn_used_when_bed_still_stands() {
        let (mut sim, id, epoch, _rx) = setup();
        // Place a bed on the floor and claim it.
        assert!(sim.world.place_multitile(60, FLOOR - 2, TileId::Bed));
        place_player(&mut sim, id, 58.0, FLOOR as f32);
        sim.set_bed_spawn(id, 61, FLOOR - 2);
        assert_eq!(sim.players[&id].bed_spawn, Some((60, FLOOR - 2)));

        sim.hurt_player(id, 1000, Hurt::Raw);
        advance(&mut sim, RESPAWN_SECS * 60 + 1);
        msg(&mut sim, id, epoch, ClientMessage::Respawn);
        let c = sim.players[&id].center();
        assert!((c.0 - 62.0).abs() < 1.0, "respawned at the bed ({c:?})");

        // Smash the bed: next death falls back to the world spawn.
        sim.break_tile(60, FLOOR - 2);
        sim.hurt_player(id, 1000, Hurt::Raw);
        advance(&mut sim, RESPAWN_SECS * 60 + 1);
        msg(&mut sim, id, epoch, ClientMessage::Respawn);
        let world_spawn = spawn_pos(sim.world());
        assert!((sim.players[&id].pos.0 - world_spawn.0).abs() < 0.01);
    }

    // ---- Debuffs, potions, regen ----------------------------------------------------

    #[test]
    fn burning_ticks_and_expires() {
        let (mut sim, id, _epoch, mut rx) = setup();
        sim.add_debuff(id, Debuff::Burning, 2 * TICK_RATE); // 2 s
        assert!(sim.has_debuff(id, Debuff::Burning));
        assert!(drain(&mut rx).iter().any(|m| matches!(m,
            ServerMessage::PlayerDebuffs { debuffs, .. } if !debuffs.is_empty())));
        advance(&mut sim, 2 * TICK_RATE + 2);
        // §8: 2 dmg/s ignoring defense → 4 HP over 2 s.
        assert_eq!(sim.players[&id].hp, 96);
        assert!(!sim.has_debuff(id, Debuff::Burning), "expired");
        assert!(drain(&mut rx).iter().any(|m| matches!(m,
            ServerMessage::PlayerDebuffs { debuffs, .. } if debuffs.is_empty())));
    }

    #[test]
    fn healing_potion_heals_and_sickness_blocks_the_second() {
        let (mut sim, id, epoch, _rx) = setup();
        give(&mut sim, id, 0, ItemId::LesserHealingPotion, 5);
        sim.hurt_player(id, 60, Hurt::Raw);
        assert_eq!(sim.players[&id].hp, 40);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::UseItem {
                slot: 0,
                aim: (0.0, 0.0),
            },
        );
        assert_eq!(sim.players[&id].hp, 90, "+50 (§4.4)");
        assert!(sim.has_debuff(id, Debuff::PotionSickness));
        assert_eq!(sim.players[&id].inventory[0].map(|s| s.count), Some(4));
        // Second sip blocked by Potion Sickness.
        advance(&mut sim, 40);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::UseItem {
                slot: 0,
                aim: (0.0, 0.0),
            },
        );
        assert_eq!(sim.players[&id].hp, 90);
        assert_eq!(sim.players[&id].inventory[0].map(|s| s.count), Some(4));
    }

    #[test]
    fn life_crystal_raises_max_hp_to_the_cap() {
        let (mut sim, id, epoch, _rx) = setup();
        give(&mut sim, id, 0, ItemId::LifeCrystal, 20);
        for _ in 0..16 {
            msg(
                &mut sim,
                id,
                epoch,
                ClientMessage::UseItem {
                    slot: 0,
                    aim: (0.0, 0.0),
                },
            );
            advance(&mut sim, 20);
        }
        let p = &sim.players[&id];
        assert_eq!(p.max_hp, 400, "§8 cap");
        assert_eq!(p.hp, 400);
        // 15 crystals consumed (the 16th refused at the cap).
        assert_eq!(p.inventory[0].map(|s| s.count), Some(5));
    }

    #[test]
    fn regen_starts_after_8s_and_band_of_vigor_stacks() {
        let (mut sim, id, _epoch, _rx) = setup();
        sim.hurt_player(id, 10, Hurt::Raw);
        assert_eq!(sim.players[&id].hp, 90);
        // Inside the 8 s window: nothing.
        advance(&mut sim, 7 * 60);
        assert_eq!(sim.players[&id].hp, 90);
        // 4 s past the delay at 0.5 HP/s (anim isn't GROUNDED → no ×2): +2.
        advance(&mut sim, 5 * 60);
        assert_eq!(sim.players[&id].hp, 92);
        // Band of Vigor: +0.5 → 1.0 HP/s.
        {
            let p = sim.players.get_mut(&id).expect("p");
            p.inventory[inventory::ACCESSORY_START] = Some(InvSlot::new(ItemId::BandOfVigor, 1));
        }
        advance(&mut sim, 4 * 60);
        assert_eq!(sim.players[&id].hp, 96);
    }

    #[test]
    fn lava_contact_damages_burns_and_obsidian_charm_halves() {
        let (mut sim, id, _epoch, _rx) = setup();
        // A lava cell overlapping the player's feet.
        let (x, y) = (50u32, FLOOR - 1);
        let mut t = Tile::AIR;
        t.liquid = Liquid::new(LiquidKind::Lava, 8);
        sim.change_tile(x, y, t);
        sim.change_tile(x - 1, y, Tile::of(TileId::Stone));
        sim.change_tile(x + 1, y, Tile::of(TileId::Stone));
        place_player(&mut sim, id, 50.5, FLOOR as f32);
        advance(&mut sim, 1);
        // §3: 50 contact damage (vs 0 defense) + Burning.
        assert_eq!(sim.players[&id].hp, 50);
        assert!(sim.has_debuff(id, Debuff::Burning));

        // Obsidian Charm: half lava damage, Burning-immune.
        let mut sim2 = flat_sim(100, 100, FLOOR);
        let (id2, _e2, _rx2) = join(&mut sim2, "obsidian");
        {
            let p = sim2.players.get_mut(&id2).expect("p");
            p.inventory[inventory::ACCESSORY_START] = Some(InvSlot::new(ItemId::ObsidianCharm, 1));
        }
        let mut t = Tile::AIR;
        t.liquid = Liquid::new(LiquidKind::Lava, 8);
        sim2.change_tile(x, y, t);
        sim2.change_tile(x - 1, y, Tile::of(TileId::Stone));
        sim2.change_tile(x + 1, y, Tile::of(TileId::Stone));
        place_player(&mut sim2, id2, 50.5, FLOOR as f32);
        advance(&mut sim2, 1);
        assert_eq!(sim2.players[&id2].hp, 75, "halved to 25");
        assert!(!sim2.has_debuff(id2, Debuff::Burning), "burn-immune");
    }

    #[test]
    fn dead_players_cannot_act_on_the_world() {
        let (mut sim, id, epoch, _rx) = setup();
        sim.hurt_player(id, 1000, Hurt::Raw);
        assert!(sim.players[&id].dead);
        // A world swing from a corpse is dropped at dispatch.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::HitTile { x: 50, y: FLOOR },
        );
        assert_eq!(sim.world().tile(50, FLOOR).id, TileId::Stone);
        assert!(sim.tile_damage.is_empty());
    }
}
