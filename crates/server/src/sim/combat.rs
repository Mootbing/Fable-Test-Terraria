//! Combat: the `UseItem` weapon paths (§4.1 melee swings and bows), enemy
//! damage with §0 crits/defense/knockback-resist/i-frames, projectile
//! flight (arrows, Void Sickles), and enemy-contact damage to players.
//!
//! Damage *to players* funnels through `sim::survival::hurt_player`
//! (defense, §0 i-frames, knockback message, death).

use ferraria_shared::enemies::{self as ed, EnemyKind, KNOCKBACK_UP_MULT, PLAYER_KNOCKBACK_SPEED};
use ferraria_shared::items::{
    inventory, Consumable, InvSlot, ItemId, WeaponKind, ARROW_GRAVITY_MULT, ARROW_LIFETIME_SECS,
    ARROW_RECOVER_CHANCE, ARROW_SPEED, EMBER_BLADE_BURN_CHANCE, EMBER_BLADE_BURN_SECS,
    FLAMING_ARROW_BURN_CHANCE, FLAMING_ARROW_BURN_SECS, MELEE_ARC_TILES,
};
use ferraria_shared::physics::{step_flier_body, GRAVITY, TERMINAL_VELOCITY};
use ferraria_shared::protocol::{Debuff, DespawnReason, ServerMessage};
use ferraria_shared::{damage_dealt, CRIT_CHANCE, CRIT_MULT, DT, ENEMY_IFRAME_TICKS, TICK_RATE};

use super::entities::{Entity, EntityKind};
use super::game::Sim;
use super::survival::Hurt;

/// A damage source for the §0 per-source enemy i-frame keyspace. Player
/// ids and entity ids are independent counters that both start at 1, so a
/// raw u32 would alias a player's melee window with an unrelated
/// projectile's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DamageSource {
    /// A player's melee arc, by player id.
    Player(u32),
    /// A projectile in flight, by entity id.
    Projectile(u32),
}

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
        let attack =
            (w.damage as f32 * effects.damage_mult * effects.melee_damage_mult).round() as u32;
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
                self.hurt_enemy(
                    eid,
                    s.attack,
                    DamageSource::Player(pid),
                    s.knockback,
                    s.facing as f32,
                    s.burn,
                );
            }
        }
    }

    /// Damages one enemy from `source`: §0 per-source i-frames, the §0
    /// damage formula vs the enemy's defense, the crit roll (×2 the dealt
    /// damage, §0), §5.1 knockback-resist-scaled knockback, aggro, and
    /// death.
    pub(crate) fn hurt_enemy(
        &mut self,
        eid: u32,
        attack: u32,
        source: DamageSource,
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
        let mut damage = damage_dealt(attack, data.defense as u32);
        if crit {
            // §0: a crit is ×2 the *dealt* damage (after defense).
            damage = (damage as f32 * CRIT_MULT) as u32;
        }
        e.hp = e.hp.saturating_sub(damage.min(u16::MAX as u32) as u16);
        e.hp_dirty = true;
        e.awake = true;
        // §5.1: damage permanently ends day passivity ("until damaged").
        e.ai.passive = false;
        e.ai.aggroed = true;
        // Knockback scaled by resist (−20% resist = 20% extra, §5.1).
        let scale = (1.0 - data.kb_resist).max(0.0);
        if knockback > 0.0 && scale > 0.0 {
            e.vel.0 = dir.signum() * knockback * scale;
            e.vel.1 = e.vel.1.min(-knockback * scale * KNOCKBACK_UP_MULT);
        }
        if let Some((chance, secs)) = burn {
            if self.loot_rng.chance(chance) {
                e.ai.burn_ticks = e.ai.burn_ticks.max((secs * TICK_RATE as f32) as u32);
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
                s.item
                    .data()
                    .weapon
                    .is_some_and(|w| w.kind == WeaponKind::Arrow)
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
        let attack =
            ((bow_stats.damage + arrow_damage) as f32 * effects.damage_mult).round() as u16;
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
            let spread = (i as f32 - (ed::SWOOPER_VOLLEY_COUNT - 1) as f32 / 2.0)
                * ed::SWOOPER_VOLLEY_SPREAD_RAD;
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
            .filter(|(_, e)| matches!(e.kind, EntityKind::Arrow { .. } | EntityKind::VoidSickle))
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
        let lifetime_over = self.tick.saturating_sub(e.spawn_tick)
            >= (ARROW_LIFETIME_SECS * TICK_RATE as f32) as u64;
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
            self.hurt_enemy(
                eid,
                attack as u32,
                DamageSource::Projectile(id),
                knockback,
                dir,
                burn,
            );
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
                Hurt::Hit {
                    knockback: Some(kb),
                },
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
                    Hurt::Hit {
                        knockback: Some(kb),
                    },
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

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;
    use ferraria_shared::enemies::SpawnEnvironment;
    use ferraria_shared::protocol::ClientMessage;
    use ferraria_shared::rng::Pcg32;

    const FLOOR: u32 = 30;

    fn hostile(sim: &mut Sim, kind: EnemyKind, x: f32) -> u32 {
        sim.spawn_enemy(
            kind,
            {
                let (w, h) = kind.data().size;
                (x - w / 2.0, FLOOR as f32 - h - 1e-3)
            },
            SpawnEnvironment::SurfaceNight, // hostile regardless of time
        )
    }

    /// Sim (night, so nothing dawn-flees mid-test) + one player standing at
    /// x=50 on the floor.
    fn setup() -> (
        Sim,
        u32,
        u64,
        tokio::sync::mpsc::Receiver<super::super::game::Frame>,
    ) {
        let mut sim = flat_sim(100, 60, FLOOR);
        sim.world.time = 0;
        let (id, epoch, mut rx) = join(&mut sim, "fighter");
        drain(&mut rx);
        place_player(&mut sim, id, 50.0, FLOOR as f32);
        (sim, id, epoch, rx)
    }

    #[test]
    fn melee_swing_damages_with_defense_crit_and_knockback() {
        let (mut sim, id, epoch, mut rx) = setup();
        give(&mut sim, id, 0, ItemId::WoodSword, 1); // 7 dmg, kb 5 (§4.1)
        let eid = hostile(&mut sim, EnemyKind::BlueSlime, 52.0); // def 2, kb 0%
        let hp0 = sim.entities.map[&eid].hp;
        drain(&mut rx);

        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::UseItem {
                slot: 0,
                aim: (53.0, FLOOR as f32 - 1.0),
            },
        );
        advance(&mut sim, 1);
        let hurt = drain(&mut rx)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::EntityHurt { id, damage, crit } if id == eid => Some((damage, crit)),
                _ => None,
            })
            .expect("EntityHurt broadcast");
        // §0: damage = max(1, 7 − floor(2/2)) = 6, ×2 on crit (12).
        let expect = if hurt.1 {
            damage_dealt(7, 2) * 2
        } else {
            damage_dealt(7, 2)
        };
        assert_eq!(hurt.0, expect);
        let e = sim.entities.map[&eid];
        assert_eq!(e.hp, hp0 - expect as u16);
        // Knockback: kb 5, resist 0% → vx exactly +5, popped upward.
        assert!((e.vel.0 - 5.0).abs() < 1e-3, "vx {}", e.vel.0);
        assert!(e.vel.1 <= -5.0 * KNOCKBACK_UP_MULT + 1e-3, "vy {}", e.vel.1);
        assert!(!e.ai.passive, "damage aggros");
    }

    #[test]
    fn kb_resist_scales_and_negative_resist_amplifies() {
        let (mut sim, _id, _epoch, _rx) = setup();
        // Green Slime: −20% resist → ×1.2. Zombie: 50% → ×0.5.
        let green = hostile(&mut sim, EnemyKind::GreenSlime, 60.0);
        let zombie = hostile(&mut sim, EnemyKind::Zombie, 70.0);
        sim.hurt_enemy(green, 1, DamageSource::Player(1), 5.0, 1.0, None);
        sim.hurt_enemy(zombie, 1, DamageSource::Player(1), 5.0, -1.0, None);
        let g = sim.entities.map[&green].vel;
        let z = sim.entities.map[&zombie].vel;
        assert!((g.0 - 6.0).abs() < 1e-3, "green vx {} (5 × 1.2)", g.0);
        assert!((z.0 + 2.5).abs() < 1e-3, "zombie vx {} (−5 × 0.5)", z.0);
    }

    #[test]
    fn enemy_iframes_block_same_source_but_not_others() {
        let (mut sim, _id, _epoch, _rx) = setup();
        let eid = hostile(&mut sim, EnemyKind::Zombie, 60.0);
        let hp0 = sim.entities.map[&eid].hp;
        sim.hurt_enemy(eid, 10, DamageSource::Player(1), 0.0, 1.0, None);
        let hp1 = sim.entities.map[&eid].hp;
        assert!(hp1 < hp0, "first hit lands");
        // Same source, immediately: blocked.
        sim.hurt_enemy(eid, 10, DamageSource::Player(1), 0.0, 1.0, None);
        assert_eq!(sim.entities.map[&eid].hp, hp1, "i-frames block source 1");
        // Different source: lands.
        sim.hurt_enemy(eid, 10, DamageSource::Player(2), 0.0, 1.0, None);
        let hp2 = sim.entities.map[&eid].hp;
        assert!(hp2 < hp1, "other sources unaffected");
        // After the 10-tick window: source 1 lands again.
        sim.tick += ENEMY_IFRAME_TICKS as u64;
        sim.hurt_enemy(eid, 10, DamageSource::Player(1), 0.0, 1.0, None);
        assert!(sim.entities.map[&eid].hp < hp2, "window elapsed");
    }

    #[test]
    fn iframe_sources_distinguish_players_from_projectiles() {
        let (mut sim, _id, _epoch, _rx) = setup();
        let eid = hostile(&mut sim, EnemyKind::Zombie, 60.0);
        let hp0 = sim.entities.map[&eid].hp;
        sim.hurt_enemy(eid, 10, DamageSource::Player(5), 0.0, 1.0, None);
        let hp1 = sim.entities.map[&eid].hp;
        assert!(hp1 < hp0, "melee hit lands");
        // A projectile whose entity id equals the player id is a different
        // source: it must not be swallowed by the player's i-frame window.
        sim.hurt_enemy(eid, 10, DamageSource::Projectile(5), 0.0, 1.0, None);
        assert!(sim.entities.map[&eid].hp < hp1, "no keyspace aliasing");
    }

    #[test]
    fn crit_rate_is_about_four_percent() {
        let (mut sim, _id, _epoch, _rx) = setup();
        sim.loot_rng = Pcg32::new(1234);
        let eid = hostile(&mut sim, EnemyKind::AshDemon, 60.0);
        sim.entities.map.get_mut(&eid).expect("demon").hp = u16::MAX; // survive the sample
                                                                      // Distinct source ids sidestep the i-frame gate per call.
        let base = damage_dealt(10, EnemyKind::AshDemon.data().defense as u32);
        let crit = base * 2; // §0: ×2 the dealt damage
        let (mut crits, trials) = (0u32, 3000u32);
        for source in 0..trials {
            let before = sim.entities.map[&eid].hp;
            sim.hurt_enemy(eid, 10, DamageSource::Player(source + 100), 0.0, 1.0, None);
            let dealt = (before - sim.entities.map[&eid].hp) as u32;
            if dealt == crit {
                crits += 1;
            } else {
                assert_eq!(dealt, base, "non-crit damage");
            }
        }
        let rate = crits as f32 / trials as f32;
        assert!((0.02..0.065).contains(&rate), "crit rate {rate} (§0: 4%)");
    }

    #[test]
    fn bow_consumes_arrows_and_terrain_recovery_is_half() {
        let (mut sim, id, epoch, mut rx) = setup();
        sim.loot_rng = Pcg32::new(7);
        give(&mut sim, id, 0, ItemId::WoodenBow, 1);
        sim.players.get_mut(&id).expect("p").inventory[1] =
            Some(InvSlot::new(ItemId::WoodenArrow, 100));
        drain(&mut rx);

        let shots = 60u32;
        for _ in 0..shots {
            // Far across the map, so recovered drops land outside the
            // shooter's auto-pickup radius.
            msg(
                &mut sim,
                id,
                epoch,
                ClientMessage::UseItem {
                    slot: 0,
                    aim: (95.0, FLOOR as f32 - 1.0),
                },
            );
            advance(&mut sim, 31); // §4.1 bow use time is 0.5 s
        }
        drain(&mut rx);
        let p = &sim.players[&id];
        let left = p.inventory[1].map(|s| s.count).unwrap_or(0);
        assert_eq!(left, 100 - shots as u16, "one arrow per shot");
        // Surviving recovered drops (merges preserve the total count).
        let recovered: u32 = sim
            .entities
            .map
            .values()
            .filter_map(|e| match e.kind {
                EntityKind::ItemDrop {
                    item: ItemId::WoodenArrow,
                    count,
                } => Some(count as u32),
                _ => None,
            })
            .sum();
        let rate = recovered as f32 / shots as f32;
        assert!(
            (0.3..0.7).contains(&rate),
            "recovery rate {rate} (§4.1: 50%)"
        );
    }

    #[test]
    fn arrows_damage_enemies_with_bow_plus_arrow_attack() {
        let (mut sim, id, epoch, mut rx) = setup();
        give(&mut sim, id, 0, ItemId::WoodenBow, 1); // 4 dmg
        sim.players.get_mut(&id).expect("p").inventory[1] =
            Some(InvSlot::new(ItemId::WoodenArrow, 10)); // 5 dmg
                                                         // A tall target a few tiles away, straight ahead.
        let eid = hostile(&mut sim, EnemyKind::Zombie, 56.0);
        drain(&mut rx);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::UseItem {
                slot: 0,
                aim: (56.0, FLOOR as f32 - 1.5),
            },
        );
        advance(&mut sim, 20);
        let hurt = drain(&mut rx)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::EntityHurt { id, damage, crit } if id == eid => Some((damage, crit)),
                _ => None,
            })
            .expect("arrow hit the zombie");
        // §4.1: attack = bow 4 + arrow 5 = 9, vs the zombie's §5.1 defense
        // (×2 the dealt damage on crit, §0).
        let def = EnemyKind::Zombie.data().defense as u32;
        let expect = if hurt.1 {
            damage_dealt(9, def) * 2
        } else {
            damage_dealt(9, def)
        };
        assert_eq!(hurt.0, expect);
        // The arrow itself despawned on the hit (no terrain recovery roll).
        assert!(!sim
            .entities
            .map
            .values()
            .any(|e| matches!(e.kind, EntityKind::Arrow { .. })));
    }

    #[test]
    fn cinderbow_upgrades_wooden_arrows_to_flaming() {
        let (mut sim, id, epoch, mut rx) = setup();
        give(&mut sim, id, 0, ItemId::Cinderbow, 1);
        sim.players.get_mut(&id).expect("p").inventory[1] =
            Some(InvSlot::new(ItemId::WoodenArrow, 10));
        drain(&mut rx);
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::UseItem {
                slot: 0,
                aim: (90.0, FLOOR as f32 - 5.0),
            },
        );
        let spawned = drain(&mut rx)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::EntitySpawn { kind, .. } => Some(kind),
                _ => None,
            })
            .expect("projectile spawn");
        assert_eq!(
            spawned,
            ferraria_shared::protocol::EntityKind::FlamingArrowProjectile
        );
    }

    #[test]
    fn enemy_contact_damages_knocks_back_and_respects_iframes() {
        let (mut sim, id, epoch, mut rx) = setup();
        // Zombie standing inside the player: contact 14 vs defense 0.
        let _eid = hostile(&mut sim, EnemyKind::Zombie, 50.4);
        drain(&mut rx);
        advance(&mut sim, 1);
        assert_eq!(sim.players[&id].hp, 100 - 14);
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::PlayerHealth { id: pid, hp: 86, .. } if *pid == id)));
        assert!(msgs
            .iter()
            .any(|m| matches!(m, ServerMessage::PlayerKnockback { .. })));
        // §0 i-frames: 40 ticks of immunity even while overlapping.
        advance(&mut sim, 30);
        assert_eq!(sim.players[&id].hp, 86, "i-frames hold");
        advance(&mut sim, 60);
        assert!(sim.players[&id].hp < 86, "window elapsed → hit again");
        let _ = epoch;
    }

    #[test]
    fn passive_slimes_do_no_contact_damage_until_hit() {
        let mut sim = flat_sim(100, 60, FLOOR);
        let (id, _epoch, mut rx) = join(&mut sim, "alice");
        drain(&mut rx);
        place_player(&mut sim, id, 50.0, FLOOR as f32);
        // Passive (surface day) slime on the player.
        let eid = sim.spawn_enemy(
            EnemyKind::GreenSlime,
            (49.5, FLOOR as f32 - 2.0),
            SpawnEnvironment::SurfaceDay,
        );
        advance(&mut sim, 30);
        assert_eq!(sim.players[&id].hp, 100, "passive slime is harmless");
        // Hit it once: aggro.
        sim.hurt_enemy(eid, 1, DamageSource::Player(id), 0.0, 1.0, None);
        assert!(!sim.entities.map[&eid].ai.passive);
        // Drop it back onto the player and let contact land.
        if let Some(e) = sim.entities.map.get_mut(&eid) {
            e.pos = (49.5, FLOOR as f32 - 2.0);
            e.vel = (0.0, 0.0);
        }
        advance(&mut sim, 10);
        assert!(sim.players[&id].hp < 100, "aggroed slime hurts");
    }

    #[test]
    fn ash_demon_volley_fires_sickles_with_line_of_sight() {
        let mut sim = flat_sim(160, 80, 60);
        sim.world.time = 0;
        let (id, _epoch, mut rx) = join(&mut sim, "alice");
        drain(&mut rx);
        place_player(&mut sim, id, 80.0, 60.0);
        // Hovering demon with line of sight; volley cooldown starts at 0.
        sim.spawn_enemy(
            EnemyKind::AshDemon,
            (70.0, 50.0),
            SpawnEnvironment::Underworld,
        );
        advance(&mut sim, 3);
        let sickles = sim
            .entities
            .map
            .values()
            .filter(|e| matches!(e.kind, EntityKind::VoidSickle))
            .count() as u32;
        assert_eq!(sickles, ed::SWOOPER_VOLLEY_COUNT, "§5.2: a volley of 4");
        assert!(drain(&mut rx).iter().any(|m| matches!(
            m,
            ServerMessage::EntitySpawn {
                kind: ferraria_shared::protocol::EntityKind::VoidSickleProjectile,
                ..
            }
        )));
    }
}
