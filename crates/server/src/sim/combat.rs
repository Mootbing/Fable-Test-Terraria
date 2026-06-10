//! Combat: the `UseItem` weapon paths (§4.1 melee swings and bows), enemy
//! damage with §0 crits/defense/knockback-resist/i-frames, projectile
//! flight (arrows, Void Sickles), and enemy-contact damage to players.
//!
//! Damage *to players* funnels through `sim::survival::hurt_player`
//! (defense, §0 i-frames, knockback message, death).

use ferraria_shared::enemies::{
    self as ed, EnemyKind, KNOCKBACK_UP_MULT, PLAYER_KNOCKBACK_SPEED,
};
use ferraria_shared::items::{
    inventory, Consumable, InvSlot, ItemId, WeaponKind, ARROW_GRAVITY_MULT, ARROW_LIFETIME_SECS,
    ARROW_RECOVER_CHANCE, ARROW_SPEED, EMBER_BLADE_BURN_CHANCE, EMBER_BLADE_BURN_SECS,
    FLAMING_ARROW_BURN_CHANCE, FLAMING_ARROW_BURN_SECS, MELEE_ARC_TILES,
};
use ferraria_shared::physics::{step_flier_body, GRAVITY, TERMINAL_VELOCITY};
use ferraria_shared::protocol::{Debuff, DespawnReason, ServerMessage};
use ferraria_shared::{
    damage_dealt, CRIT_CHANCE, CRIT_MULT, DT, ENEMY_IFRAME_TICKS, TICK_RATE,
};

use super::entities::{Entity, EntityKind};
use super::game::Sim;
use super::survival::Hurt;

/// One active melee swing: the §4.1 3×3-tile arc stays live for the swing
/// duration, hitting each enemy at most once per `ENEMY_IFRAME_TICKS`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MeleeSwing {
    pub until_tick: u64,
    /// Attack with the player's §4.2/§4.3 multipliers applied (pre-crit).
    pub attack: u32,
    pub knockback: f32,
    pub facing: i8,
    /// Burning proc on hit: (chance, seconds) — Ember Blade.
    pub burn: Option<(f32, f32)>,
}

/// Stale i-frame entries are purged on this cadence.
const IFRAME_PURGE_TICKS: u64 = 600;

impl Sim {
    /// `UseItem`: weapon swing / bow shot / consumable, server-validated.
    /// `aim` is in world tile coordinates.
    pub(crate) fn use_item(&mut self, id: u32, slot: u8, aim: (f32, f32)) {
        if slot as usize >= inventory::HOTBAR {
            return;
        }
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if p.dead {
            return;
        }
        let Some(stack) = p.inventory.get(slot as usize).copied().flatten() else {
            return;
        };
        let item = stack.item;
        let data = item.data();

        // Server-enforced use rate from the item's use time (the same
        // limiter as mining swings, so tool users can't interleave a free
        // extra arc; ±1 tick of network-phase jitter tolerated).
        let use_ticks = {
            let secs = data
                .tool
                .map(|t| t.use_secs)
                .or(data.weapon.map(|w| w.use_secs))
                .unwrap_or(ferraria_shared::items::BARE_HAND_USE_SECS);
            ((secs * TICK_RATE as f32).round() as u64).max(1)
        };
        if let Some(last) = p.last_swing_tick {
            if tick.saturating_sub(last) + 1 < use_ticks {
                return;
            }
        }

        match (data.weapon.map(|w| w.kind), data.consumable) {
            (Some(WeaponKind::Melee), _) => {
                p.last_swing_tick = Some(tick);
                self.start_melee_swing(id, item, aim, use_ticks);
            }
            (Some(WeaponKind::Bow), _) => {
                p.last_swing_tick = Some(tick);
                self.fire_bow(id, item, aim);
            }
            (_, Some(Consumable::Heal(hp))) => {
                p.last_swing_tick = Some(tick);
                self.drink_healing(id, slot, hp);
            }
            (_, Some(Consumable::MaxHpUp(add))) => {
                p.last_swing_tick = Some(tick);
                self.use_life_crystal(id, slot, add);
            }
            (_, Some(Consumable::SummonBoss(_))) => {
                // INTEGRATE(boss-summon): the boss branch implements summon
                // validation (night-only, one alive) + the boss entities.
                tracing::debug!(player = id, ?item, "boss summons not on this branch");
            }
            (_, Some(Consumable::TeleportToSpawn)) => {
                // Warp Mirror's 1 s channel lands with the polish pass.
                tracing::debug!(player = id, "warp mirror not implemented yet");
            }
            _ => {}
        }
    }

    /// Registers the §4.1 melee arc: 3×3 tiles in the facing direction for
    /// the swing duration (`tick_swings` applies it each tick).
    fn start_melee_swing(&mut self, id: u32, item: ItemId, aim: (f32, f32), use_ticks: u64) {
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let Some(w) = item.data().weapon else {
            return;
        };
        let effects = ferraria_shared::loadout::effect_mods(&p.inventory);
        let attack = (w.damage as f32 * effects.damage_mult * effects.melee_damage_mult).round()
            as u32;
        let facing = if aim.0 >= p.center().0 { 1 } else { -1 };
        let burn = (item == ItemId::EmberBlade)
            .then_some((EMBER_BLADE_BURN_CHANCE, EMBER_BLADE_BURN_SECS));
        p.swing = Some(MeleeSwing {
            until_tick: tick + use_ticks,
            attack,
            knockback: w.knockback,
            facing,
            burn,
        });
    }

    /// Applies every live melee arc to the enemies it overlaps (per-source
    /// i-frames keep one swing from machine-gunning, §0).
    pub(crate) fn tick_swings(&mut self) {
        let tick = self.tick;
        let swings: Vec<(u32, (f32, f32), MeleeSwing)> = self
            .players
            .iter()
            .filter_map(|(&pid, p)| {
                let s = p.swing?;
                if tick >= s.until_tick || p.dead {
                    return None;
                }
                Some((pid, p.center(), s))
            })
            .collect();
        // Drop expired swings.
        for p in self.players.values_mut() {
            if p.swing.is_some_and(|s| tick >= s.until_tick) {
                p.swing = None;
            }
        }
        for (pid, center, s) in swings {
            // §4.1: 3×3-tile arc in the facing direction, vertically
            // centered on the player.
            let (x0, x1) = if s.facing > 0 {
                (center.0, center.0 + MELEE_ARC_TILES)
            } else {
                (center.0 - MELEE_ARC_TILES, center.0)
            };
            let (y0, y1) = (
                center.1 - MELEE_ARC_TILES / 2.0,
                center.1 + MELEE_ARC_TILES / 2.0,
            );
            let hits: Vec<u32> = self
                .entities
                .map
                .iter()
                .filter(|(_, e)| matches!(e.kind, EntityKind::Enemy(_)))
                .filter(|(_, e)| {
                    let (w, h) = e.size();
                    e.pos.0 < x1 && e.pos.0 + w > x0 && e.pos.1 < y1 && e.pos.1 + h > y0
                })
                .map(|(&eid, _)| eid)
                .collect();
            for eid in hits {
                self.hurt_enemy(eid, s.attack, pid, s.knockback, s.facing as f32, s.burn);
            }
        }
    }

    /// Damages one enemy from `source` (player id for melee, projectile
    /// entity id for arrows): §0 per-source i-frames, crit roll, the §0
    /// damage formula vs the enemy's defense, §5.1 knockback-resist-scaled
    /// knockback, aggro, and death.
    pub(crate) fn hurt_enemy(
        &mut self,
        eid: u32,
        attack: u32,
        source: u32,
        knockback: f32,
        dir: f32,
        burn: Option<(f32, f32)>,
    ) {
        let tick = self.tick;
        if self
            .enemy_iframes
            .get(&(eid, source))
            .is_some_and(|&until| tick < until)
        {
            return;
        }
        let Some(e) = self.entities.map.get_mut(&eid) else {
            return;
        };
        let EntityKind::Enemy(kind) = e.kind else {
            return;
        };
        self.enemy_iframes
            .insert((eid, source), tick + ENEMY_IFRAME_TICKS as u64);
        let data = kind.data();
        let crit = self.loot_rng.chance(CRIT_CHANCE);
        let attack = if crit {
            (attack as f32 * CRIT_MULT) as u32
        } else {
            attack
        };
        let damage = damage_dealt(attack, data.defense as u32);
        e.hp = e.hp.saturating_sub(damage.min(u16::MAX as u32) as u16);
        e.hp_dirty = true;
        e.awake = true;
        e.ai.passive = false; // §5.1: passive only until damaged
        // Knockback scaled by resist (−20% resist = 20% extra, §5.1).
        let scale = (1.0 - data.kb_resist).max(0.0);
        if knockback > 0.0 && scale > 0.0 {
            e.vel.0 = dir.signum() * knockback * scale;
            e.vel.1 = e.vel.1.min(-knockback * scale * KNOCKBACK_UP_MULT);
        }
        if let Some((chance, secs)) = burn {
            if self.loot_rng.chance(chance) {
                e.ai.burn_ticks = e
                    .ai
                    .burn_ticks
                    .max((secs * TICK_RATE as f32) as u32);
            }
        }
        let center = e.center();
        let dead = e.hp == 0;
        self.broadcast_at(
            center.0.max(0.0) as u32,
            center.1.max(0.0) as u32,
            &ServerMessage::EntityHurt {
                id: eid,
                damage,
                crit,
            },
        );
        if dead {
            self.kill_enemy(eid);
        }
    }

    /// Fires the held bow (§4.1): consumes the first arrow stack in the
    /// carry slots, spawns the projectile at 35 t/s toward `aim`. The
    /// Cinderbow upgrades wooden arrows to flaming in flight.
    fn fire_bow(&mut self, id: u32, bow: ItemId, aim: (f32, f32)) {
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let Some(bow_stats) = bow.data().weapon else {
            return;
        };
        // First arrow stack in slot order (hotbar then backpack).
        let Some(arrow_idx) = (0..inventory::ARMOR_START).find(|&i| {
            p.inventory[i].is_some_and(|s| {
                s.item.data().weapon.is_some_and(|w| w.kind == WeaponKind::Arrow)
            })
        }) else {
            return; // no ammo
        };
        let Some(stack) = p.inventory[arrow_idx] else {
            return;
        };
        let mut fired = stack.item;
        if bow == ItemId::Cinderbow && fired == ItemId::WoodenArrow {
            fired = ItemId::FlamingArrow; // §4.1 Cinderbow upgrade
        }
        // Consume one arrow.
        let left = stack.count - 1;
        p.inventory[arrow_idx] = (left > 0).then_some(InvSlot::new(stack.item, left));
        let new_stack = p.inventory[arrow_idx];

        let effects = ferraria_shared::loadout::effect_mods(&p.inventory);
        let arrow_damage = fired.data().weapon.map(|w| w.damage).unwrap_or(0);
        let attack = ((bow_stats.damage + arrow_damage) as f32 * effects.damage_mult).round()
            as u16;
        let center = p.center();
        let d = {
            let (dx, dy) = (aim.0 - center.0, aim.1 - center.1);
            let l = (dx * dx + dy * dy).sqrt().max(1e-3);
            (dx / l, dy / l)
        };
        let (w, h) = ferraria_shared::physics::hitbox::ARROW;
        let entity = Entity::plain(
            (center.0 - w / 2.0, center.1 - h / 2.0),
            (d.0 * ARROW_SPEED, d.1 * ARROW_SPEED),
            EntityKind::Arrow {
                item: fired,
                attack,
                knockback: bow_stats.knockback,
                owner: id,
            },
            tick,
        );
        self.send_to(
            id,
            &ServerMessage::SlotChanged {
                idx: arrow_idx as u8,
                stack: new_stack,
            },
        );
        let eid = self.entities.insert(entity);
        let msg = super::entities::spawn_message(eid, &entity);
        self.broadcast_at(center.0.max(0.0) as u32, center.1.max(0.0) as u32, &msg);
    }

    /// Spawns the §5.2 Ash Demon volley: 4 Void Sickles fanned at the
    /// target, starting at 6 t/s (they accelerate in flight).
    pub(crate) fn fire_void_volley(&mut self, from: (f32, f32), at: (f32, f32)) {
        let base = (at.1 - from.1).atan2(at.0 - from.0);
        let (w, h) = ferraria_shared::physics::hitbox::VOID_SICKLE;
        for i in 0..ed::SWOOPER_VOLLEY_COUNT {
            let spread = (i as f32 - (ed::SWOOPER_VOLLEY_COUNT - 1) as f32 / 2.0) * 0.09;
            let a = base + spread;
            let entity = Entity::plain(
                (from.0 - w / 2.0, from.1 - h / 2.0),
                (
                    a.cos() * ed::VOID_SICKLE_START_SPEED,
                    a.sin() * ed::VOID_SICKLE_START_SPEED,
                ),
                EntityKind::VoidSickle,
                self.tick,
            );
            let eid = self.entities.insert(entity);
            let msg = super::entities::spawn_message(eid, &entity);
            self.broadcast_at(from.0.max(0.0) as u32, from.1.max(0.0) as u32, &msg);
        }
    }

    /// Projectile flight + hits: arrows (gravity ×0.35, 5 s lifetime, tile
    /// hits recover 50% as drops, enemy hits damage with the bow's
    /// knockback) and Void Sickles (accelerating, tile-destroyed,
    /// player-damaging with the §5.2 Darkness proc).
    pub(crate) fn tick_projectiles(&mut self) {
        let ids: Vec<u32> = self
            .entities
            .map
            .iter()
            .filter(|(_, e)| {
                matches!(e.kind, EntityKind::Arrow { .. } | EntityKind::VoidSickle)
            })
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            let Some(e) = self.entities.map.get(&id) else {
                continue;
            };
            match e.kind {
                EntityKind::Arrow { .. } => self.step_arrow(id),
                EntityKind::VoidSickle => self.step_sickle(id),
                _ => {}
            }
        }
    }

    fn step_arrow(&mut self, id: u32) {
        let Some(e) = self.entities.map.get_mut(&id) else {
            return;
        };
        let EntityKind::Arrow {
            item,
            attack,
            knockback,
            ..
        } = e.kind
        else {
            return;
        };
        // §4.1: 35 t/s launch, gravity ×0.35, despawn after 5 s.
        e.vel.1 = (e.vel.1 + GRAVITY * ARROW_GRAVITY_MULT * DT).min(TERMINAL_VELOCITY);
        let size = e.size();
        let step = step_flier_body(&self.world, &mut e.pos, &mut e.vel, size, DT);
        e.awake = true;
        let center = e.center();
        let lifetime_over =
            self.tick.saturating_sub(e.spawn_tick) >= (ARROW_LIFETIME_SECS * TICK_RATE as f32) as u64;
        let hit_tile = step.blocked_x || step.on_ground || step.hit_ceiling;
        let dir = e.vel.0;

        if hit_tile {
            // §4.1: 50% chance to recover the arrow from terrain.
            self.despawn_entity(id, DespawnReason::Despawned);
            if self.loot_rng.chance(ARROW_RECOVER_CHANCE) {
                self.spawn_item_drop(item, 1, center);
            }
            return;
        }
        if lifetime_over {
            self.despawn_entity(id, DespawnReason::Despawned);
            return;
        }
        // Enemy hit: first overlapping enemy in id order.
        let hit: Option<u32> = self
            .entities
            .map
            .iter()
            .filter(|(_, t)| matches!(t.kind, EntityKind::Enemy(_)))
            .find(|(_, t)| {
                let (w, h) = t.size();
                let (aw, ah) = size;
                let apos = (center.0 - aw / 2.0, center.1 - ah / 2.0);
                apos.0 < t.pos.0 + w
                    && apos.0 + aw > t.pos.0
                    && apos.1 < t.pos.1 + h
                    && apos.1 + ah > t.pos.1
            })
            .map(|(&eid, _)| eid);
        if let Some(eid) = hit {
            let burn = (item == ItemId::FlamingArrow)
                .then_some((FLAMING_ARROW_BURN_CHANCE, FLAMING_ARROW_BURN_SECS));
            self.hurt_enemy(eid, attack as u32, id, knockback, dir, burn);
            self.despawn_entity(id, DespawnReason::Killed);
        }
    }

    fn step_sickle(&mut self, id: u32) {
        let Some(e) = self.entities.map.get_mut(&id) else {
            return;
        };
        // §5.2: starts 6 t/s, accelerates at 15 t/s² up to 25 t/s.
        let speed = (e.vel.0 * e.vel.0 + e.vel.1 * e.vel.1).sqrt();
        if speed > 1e-3 {
            let new_speed = (speed + ed::VOID_SICKLE_ACCEL * DT).min(ed::VOID_SICKLE_MAX_SPEED);
            let s = new_speed / speed;
            e.vel.0 *= s;
            e.vel.1 *= s;
        }
        let size = e.size();
        let step = step_flier_body(&self.world, &mut e.pos, &mut e.vel, size, DT);
        e.awake = true;
        let pos = e.pos;
        let vel = e.vel;
        let lifetime_over = self.tick.saturating_sub(e.spawn_tick)
            >= (ed::VOID_SICKLE_LIFETIME_SECS * TICK_RATE as f32) as u64;
        if step.blocked_x || step.on_ground || step.hit_ceiling || lifetime_over {
            // §5.2: destroyed by tiles.
            self.despawn_entity(id, DespawnReason::Killed);
            return;
        }
        // Player hit.
        let hit: Option<u32> = self
            .players
            .iter()
            .filter(|(_, p)| !p.dead)
            .find(|(_, p)| {
                let (pw, ph) = ferraria_shared::physics::hitbox::PLAYER;
                pos.0 < p.pos.0 + pw
                    && pos.0 + size.0 > p.pos.0
                    && pos.1 < p.pos.1 + ph
                    && pos.1 + size.1 > p.pos.1
            })
            .map(|(&pid, _)| pid);
        if let Some(pid) = hit {
            let kb = (
                vel.0.signum() * PLAYER_KNOCKBACK_SPEED,
                -PLAYER_KNOCKBACK_SPEED * KNOCKBACK_UP_MULT,
            );
            let applied = self.hurt_player(
                pid,
                ed::VOID_SICKLE_DAMAGE as u32,
                Hurt::Hit { knockback: Some(kb) },
            );
            if applied {
                if self.loot_rng.chance(ed::VOID_SICKLE_DARKNESS_CHANCE) {
                    self.add_debuff(
                        pid,
                        Debuff::Darkness,
                        (ed::VOID_SICKLE_DARKNESS_SECS * TICK_RATE as f32) as u32,
                    );
                }
                self.despawn_entity(id, DespawnReason::Killed);
            }
        }
    }

    /// Enemy-contact damage to players (§5.1 contact dmg vs §4.2 defense,
    /// §0 player i-frames, knockback away from the enemy). Passive slimes
    /// don't hurt; Royal Gel Charm wearers are immune to green/blue slimes.
    pub(crate) fn tick_enemy_contact(&mut self) {
        struct Toucher {
            kind: EnemyKind,
            pos: (f32, f32),
            size: (f32, f32),
            passive: bool,
        }
        let enemies: Vec<Toucher> = self
            .entities
            .map
            .values()
            .filter_map(|e| {
                let EntityKind::Enemy(kind) = e.kind else {
                    return None;
                };
                Some(Toucher {
                    kind,
                    pos: e.pos,
                    size: e.size(),
                    passive: e.ai.passive,
                })
            })
            .collect();
        let players: Vec<u32> = self.players.keys().copied().collect();
        for pid in players {
            let Some(p) = self.players.get(&pid) else {
                continue;
            };
            if p.dead || self.tick < p.iframe_until {
                continue;
            }
            let slime_friend = p.slime_friend();
            let ppos = p.pos;
            let (pw, ph) = ferraria_shared::physics::hitbox::PLAYER;
            let pcenter = (ppos.0 + pw / 2.0, ppos.1 + ph / 2.0);
            for t in &enemies {
                if t.passive || (slime_friend && t.kind.day_passive_slime()) {
                    continue;
                }
                let overlap = ppos.0 < t.pos.0 + t.size.0
                    && ppos.0 + pw > t.pos.0
                    && ppos.1 < t.pos.1 + t.size.1
                    && ppos.1 + ph > t.pos.1;
                if !overlap {
                    continue;
                }
                let dir = if pcenter.0 >= t.pos.0 + t.size.0 / 2.0 {
                    1.0
                } else {
                    -1.0
                };
                let kb = (
                    dir * PLAYER_KNOCKBACK_SPEED,
                    -PLAYER_KNOCKBACK_SPEED * KNOCKBACK_UP_MULT,
                );
                self.hurt_player(
                    pid,
                    t.kind.data().contact_damage as u32,
                    Hurt::Hit { knockback: Some(kb) },
                );
                break; // one contact hit per tick; i-frames now hold anyway
            }
        }
    }

    /// Drops i-frame entries whose window long passed (bounded memory).
    pub(crate) fn purge_enemy_iframes(&mut self) {
        if self.tick.is_multiple_of(IFRAME_PURGE_TICKS) {
            let tick = self.tick;
            self.enemy_iframes.retain(|_, &mut until| until > tick);
        }
    }
}
