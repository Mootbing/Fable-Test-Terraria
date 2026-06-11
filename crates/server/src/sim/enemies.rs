//! Enemies: the §5.3 spawning algorithm, the §5.2 per-archetype AI systems,
//! the §5.1 despawn rule, and death drops.
//!
//! All gameplay numbers come from `shared::enemies` (ENEMY_DATA + archetype
//! constants); this module is the imperative half. Damage *to* enemies and
//! contact damage *from* enemies live in `sim::combat`.

use ferraria_shared::enemies::{
    self as ed, coin_drop_value, crowding_mult, spawn_environment, spawn_ring_offset, AiKind,
    EnemyKind, SpawnEnvironment,
};
use ferraria_shared::physics::{step_enemy_body, step_flier_body, BodyStep};
use ferraria_shared::protocol::DespawnReason;
use ferraria_shared::tiles::LiquidKind;
use ferraria_shared::world::World;
use ferraria_shared::{COPPER_PER_GOLD, COPPER_PER_SILVER, DT, MAX_LIVE_ENEMIES, TICK_RATE};

use super::entities::{spawn_message, AiState, Entity, EntityKind};
use super::game::Sim;

/// How often the (cheap, but O(enemies × players)) despawn-range sweep runs.
const DESPAWN_SWEEP_TICKS: u64 = 30;

/// Fleeing dawn enemies despawn once outside every player's *screen* rect
/// (§5.2 "despawn when off-screen") — the §5.3 inner spawn rectangle.
fn off_every_screen(sim: &Sim, center: (f32, f32)) -> bool {
    sim.players.values().all(|p| {
        let pc = p.center();
        !ed::in_spawn_safe_rect(center.0 - pc.0, center.1 - pc.1)
    })
}

impl Sim {
    /// Live enemy count (the global §0 cap input).
    pub(crate) fn live_enemies(&self) -> u32 {
        self.entities
            .map
            .values()
            .filter(|e| matches!(e.kind, EntityKind::Enemy(_)))
            .count() as u32
    }

    /// Town NPC positions for §5.3 step 3 spawn suppression.
    ///
    /// INTEGRATE(town-suppression): the NPC branch implements town NPCs;
    /// merge by returning their positions here. Until then there are no
    /// town NPCs, so suppression is a no-op.
    pub(crate) fn town_npc_positions(&self) -> &[(f32, f32)] {
        &[]
    }

    /// §5.3: one spawn evaluation per player per tick.
    pub(crate) fn tick_enemy_spawning(&mut self) {
        if self.live_enemies() >= MAX_LIVE_ENEMIES {
            return;
        }
        let ids: Vec<u32> = self.players.keys().copied().collect();
        for pid in ids {
            self.try_spawn_for_player(pid);
            if self.live_enemies() >= MAX_LIVE_ENEMIES {
                return;
            }
        }
    }

    fn try_spawn_for_player(&mut self, pid: u32) {
        let Some(p) = self.players.get(&pid) else {
            return;
        };
        if p.dead {
            return;
        }
        let center = p.center();
        let env = spawn_environment(center.1.max(0.0) as u32, self.world.is_day());
        let (base_d, mut m) = env.spawn_params();

        // Step 3: town suppression — each town NPC within 50 tiles: D ×1.5,
        // M −2; 3+ such NPCs (or M ≤ 0) → no hostile spawns.
        let mut d = base_d as f32;
        let npcs_near = self
            .town_npc_positions()
            .iter()
            .filter(|&&(nx, ny)| {
                let (dx, dy) = (nx - center.0, ny - center.1);
                dx * dx + dy * dy <= 50.0 * 50.0
            })
            .count() as u32;
        if npcs_near >= 3 {
            return;
        }
        d *= 1.5f32.powi(npcs_near as i32);
        m = m.saturating_sub(2 * npcs_near);
        if m == 0 {
            return;
        }

        // Step 2: crowding — enemies in this player's despawn rectangle.
        let c = self
            .entities
            .map
            .values()
            .filter(|e| matches!(e.kind, EntityKind::Enemy(_)))
            .filter(|e| {
                let ec = e.center();
                (ec.0 - center.0).abs() <= ed::DESPAWN_RANGE_X
                    && (ec.1 - center.1).abs() <= ed::DESPAWN_RANGE_Y
            })
            .count() as u32;
        let Some(mult) = crowding_mult(c, m) else {
            return; // C ≥ M
        };
        d *= mult;

        // Step 4: the 1-in-D roll.
        let denom = d.max(1.0) as u32;
        if self.spawn_rng.gen_range_u32(0..denom.max(1)) != 0 {
            return;
        }

        // Step 5: species by environment weights, then a placement matching
        // its archetype within the spawn ring (50 tries).
        let weights = env.species_weights();
        let w: Vec<u32> = weights.iter().map(|&(_, w)| w).collect();
        let Some(i) = self.spawn_rng.pick_weighted(&w) else {
            return;
        };
        let kind = weights[i].0;
        let player_centers: Vec<(f32, f32)> = self.players.values().map(|p| p.center()).collect();
        for _ in 0..ed::SPAWN_TRIES {
            let (dx, dy) = spawn_ring_offset(&mut self.spawn_rng);
            let (cx, cy) = (center.0 as i64 + dx as i64, center.1 as i64 + dy as i64);
            if cx < 0 || cy < 0 {
                continue;
            }
            let (x, y) = (cx as u32, cy as u32);
            if !self.world.in_bounds(x, y) {
                continue;
            }
            // Never on any player's screen (multiplayer: another player may
            // be standing right inside this player's spawn ring).
            let tile_center = (x as f32 + 0.5, y as f32 + 0.5);
            if player_centers
                .iter()
                .any(|&(px, py)| ed::in_spawn_safe_rect(tile_center.0 - px, tile_center.1 - py))
            {
                continue;
            }
            let Some(pos) = placement_for(&self.world, kind, x, y) else {
                continue;
            };
            self.spawn_enemy(kind, pos, env);
            return;
        }
    }

    /// Spawns one enemy with full HP at AABB top-left `pos`. Surface-day
    /// green/blue slimes start passive (§5.1).
    pub(crate) fn spawn_enemy(
        &mut self,
        kind: EnemyKind,
        pos: (f32, f32),
        env: SpawnEnvironment,
    ) -> u32 {
        let data = kind.data();
        let passive =
            kind.day_passive_slime() && env == SpawnEnvironment::SurfaceDay && self.world.is_day();
        let entity = Entity {
            pos,
            vel: (0.0, 0.0),
            kind: EntityKind::Enemy(kind),
            spawn_tick: self.tick,
            awake: true,
            hp: data.max_hp,
            hp_dirty: false,
            ai: AiState {
                timer: self.spawn_rng.gen_range_u32(
                    secs_ticks(ed::SLIME_IDLE_MIN_SECS)..secs_ticks(ed::SLIME_IDLE_MAX_SECS),
                ),
                dir: if self.spawn_rng.chance(0.5) { 1 } else { -1 },
                passive,
                ..AiState::default()
            },
        };
        let id = self.entities.insert(entity);
        let msg = spawn_message(id, &entity);
        self.broadcast_at(pos.0.max(0.0) as u32, pos.1.max(0.0) as u32, &msg);
        id
    }

    /// Per-tick enemy systems: archetype AI + movement, dawn flee, burning,
    /// and the periodic despawn-range sweep.
    pub(crate) fn tick_enemies(&mut self) {
        // Living targets, sampled once.
        let targets: Vec<(u32, (f32, f32), bool)> = self
            .players
            .iter()
            .filter(|(_, p)| !p.dead)
            .map(|(&id, p)| (id, p.center(), p.slime_friend()))
            .collect();
        let is_day = self.world.is_day();

        let ids: Vec<u32> = self
            .entities
            .map
            .iter()
            .filter(|(_, e)| matches!(e.kind, EntityKind::Enemy(_)))
            .map(|(&id, _)| id)
            .collect();
        let mut dead: Vec<u32> = Vec::new();
        let mut gone: Vec<u32> = Vec::new();
        for id in ids {
            let Some(e) = self.entities.map.get(&id) else {
                continue;
            };
            let EntityKind::Enemy(kind) = e.kind else {
                continue;
            };
            let mut body = *e;
            // Dawn flee (§5.2/§9): zombies and demon eyes turn away at dawn
            // and despawn once off everyone's screen.
            if is_day && kind.flees_at_dawn() {
                body.ai.fleeing = true;
            }
            // §5.1: "passive on the surface during the day until damaged;
            // always hostile at night or underground" — recompute passivity
            // from those conditions every tick, so dusk (or hopping down
            // into the underground band) angers an untouched slime and dawn
            // calms it again. Only damage (`ai.aggroed`) is permanent.
            if kind.day_passive_slime() {
                body.ai.passive = slime_passive(body.ai.aggroed, body.center().1, is_day);
            }

            // Nearest living target this enemy will chase. Royal Gel Charm
            // wearers are invisible to green/blue slimes (§4.3).
            let center = body.center();
            let target = targets
                .iter()
                .filter(|&&(_, _, slime_friend)| !(slime_friend && kind.day_passive_slime()))
                .map(|&(_, c, _)| c)
                .min_by(|a, b| {
                    let da = (a.0 - center.0).powi(2) + (a.1 - center.1).powi(2);
                    let db = (b.0 - center.0).powi(2) + (b.1 - center.1).powi(2);
                    da.total_cmp(&db)
                });

            let rng = &mut self.spawn_rng;
            match kind.data().ai {
                AiKind::Slime => step_slime(&self.world, kind, &mut body, target, rng),
                AiKind::Fighter => step_fighter(&self.world, kind, &mut body, target),
                AiKind::FlierBouncer => step_bouncer(&self.world, &mut body, target),
                AiKind::FlierErratic => step_erratic(&self.world, &mut body, target, rng),
                AiKind::FlierStraight => step_straight(&self.world, &mut body, target),
                AiKind::Swooper => {
                    if let Some(volley_at) = step_swooper(&self.world, &mut body, target, rng) {
                        let from = body.center();
                        self.fire_void_volley(from, volley_at);
                    }
                }
            }

            // Enemy burning (Ember Blade / flaming arrows): 2 dmg/s,
            // ignoring defense, one point every half second.
            if body.ai.burn_ticks > 0 {
                let interval = TICK_RATE / ed::ENEMY_BURNING_DPS;
                if body.ai.burn_ticks % interval == 0 {
                    body.hp = body.hp.saturating_sub(1);
                    body.hp_dirty = true;
                }
                body.ai.burn_ticks -= 1;
            }

            body.awake = true;
            let center = body.center();
            let hp = body.hp;
            let fled = body.ai.fleeing && off_every_screen(self, center);
            if let Some(e) = self.entities.map.get_mut(&id) {
                *e = body;
            }
            if hp == 0 {
                dead.push(id);
            } else if fled {
                gone.push(id);
            }
        }
        for id in dead {
            self.kill_enemy(id);
        }
        for id in gone {
            self.despawn_entity(id, DespawnReason::Despawned);
        }

        if self.tick.is_multiple_of(DESPAWN_SWEEP_TICKS) {
            self.despawn_far_enemies();
        }
    }

    /// §5.1: an enemy despawns once it is >168 tiles horizontal or >94
    /// vertical from *every* player (with no players online, that's all of
    /// them).
    fn despawn_far_enemies(&mut self) {
        let centers: Vec<(f32, f32)> = self.players.values().map(|p| p.center()).collect();
        let far: Vec<u32> = self
            .entities
            .map
            .iter()
            .filter(|(_, e)| matches!(e.kind, EntityKind::Enemy(_)))
            .filter(|(_, e)| {
                let c = e.center();
                !centers.iter().any(|&(px, py)| {
                    (c.0 - px).abs() <= ed::DESPAWN_RANGE_X
                        && (c.1 - py).abs() <= ed::DESPAWN_RANGE_Y
                })
            })
            .map(|(&id, _)| id)
            .collect();
        for id in far {
            self.despawn_entity(id, DespawnReason::Despawned);
        }
    }

    /// Kills an enemy: §5.1 drops (coins with ×0.8–1.2 variance + item
    /// rows) through the item-drop system, then a `Killed` despawn (clients
    /// play the death poof on that reason).
    pub(crate) fn kill_enemy(&mut self, id: u32) {
        let Some(e) = self.entities.map.get(&id) else {
            return;
        };
        let EntityKind::Enemy(kind) = e.kind else {
            return;
        };
        let center = e.center();
        let data = kind.data();
        let coins = coin_drop_value(&mut self.loot_rng, data.coins);
        self.spawn_coin_drops(coins, center);
        for row in data.drops {
            if self.loot_rng.chance(row.chance) {
                let n = self
                    .loot_rng
                    .gen_range_u32(row.min as u32..row.max as u32 + 1)
                    as u16;
                if n > 0 {
                    self.spawn_item_drop(row.item, n, center);
                }
            }
        }
        self.despawn_entity(id, DespawnReason::Killed);
    }

    /// Spawns `value` copper worth of coin drops in the largest
    /// denominations (gold/silver/copper; platinum never drops from §5
    /// enemies or §8 deaths).
    pub(crate) fn spawn_coin_drops(&mut self, value: u32, center: (f32, f32)) {
        let gold = value / COPPER_PER_GOLD;
        let silver = (value % COPPER_PER_GOLD) / COPPER_PER_SILVER;
        let copper = value % COPPER_PER_SILVER;
        for (item, n) in [
            (ferraria_shared::items::ItemId::GoldCoin, gold),
            (ferraria_shared::items::ItemId::SilverCoin, silver),
            (ferraria_shared::items::ItemId::CopperCoin, copper),
        ] {
            if n > 0 {
                self.spawn_item_drop(item, n as u16, center);
            }
        }
    }
}

/// Seconds → ticks (rounded down, min 1).
fn secs_ticks(secs: f32) -> u32 {
    ((secs * TICK_RATE as f32) as u32).max(1)
}

/// §5.1 passivity for green/blue slimes: passive iff never damaged, above
/// the underground band, and it's daytime. Read straight off the spec
/// sentence: "passive on the surface during the day **until damaged**"
/// scopes permanence to damage alone, so a slime made hostile by dusk
/// re-passivates at dawn if nobody ever hit it.
pub(crate) fn slime_passive(aggroed: bool, center_y: f32, is_day: bool) -> bool {
    !aggroed && is_day && center_y < ed::UNDERGROUND_START_ROW as f32
}

/// §5.3 step 4 placement: grounded enemies need a solid tile with 3×2 air
/// above (the enemy stands on top, horizontally centered); fliers need a
/// 2×2 air pocket. Returns the AABB top-left, or `None`.
pub(crate) fn placement_for(world: &World, kind: EnemyKind, x: u32, y: u32) -> Option<(f32, f32)> {
    let (w, h) = kind.data().size;
    if kind.data().ai.grounded() {
        if !world.is_solid(x as i32, y as i32) || y < 2 {
            return None;
        }
        // 3×2 air above the solid tile.
        for dy in 1..=2i32 {
            for dx in -1..=1i32 {
                if world.is_solid(x as i32 + dx, y as i32 - dy) {
                    return None;
                }
            }
        }
        Some((
            x as f32 + 0.5 - w / 2.0,
            y as f32 - h - ferraria_shared::physics::COLLISION_EPS,
        ))
    } else {
        for dy in 0..2i32 {
            for dx in 0..2i32 {
                if world.is_solid(x as i32 + dx, y as i32 + dy) {
                    return None;
                }
            }
        }
        Some((x as f32 + 1.0 - w / 2.0, y as f32 + 1.0 - h / 2.0))
    }
}

// ---- §5.2 archetype steppers ---------------------------------------------------
//
// Free functions over (world, body, target) so the AI is unit-testable
// without a Sim. They mutate `body` in place for one tick.

/// Slime: grounded; idle 0.7–2.0 s between hops; hop vx 5.6 toward the
/// target (vy 21, every 3rd hop 26). Passive surface slimes wander instead
/// and turn at ledges; floats on water (lava slimes on lava, bouncing 1.5×
/// higher out of it).
pub(crate) fn step_slime(
    world: &World,
    kind: EnemyKind,
    body: &mut Entity,
    target: Option<(f32, f32)>,
    rng: &mut ferraria_shared::rng::Pcg32,
) {
    let size = body.size();
    let float_on = if kind == EnemyKind::LavaSlime {
        LiquidKind::Lava
    } else {
        LiquidKind::Water
    };

    let step = step_enemy_body(world, &mut body.pos, &mut body.vel, size, DT, false);
    apply_float(body, &step, float_on, kind);

    if step.on_ground {
        // Grounded: bleed horizontal speed quickly (slimes don't slide).
        body.vel.0 *= ed::SLIME_GROUND_FRICTION;
        if body.vel.0.abs() < ed::SLIME_STOP_SPEED {
            body.vel.0 = 0.0;
        }
        if body.ai.timer > 0 {
            body.ai.timer -= 1;
            // Passive slimes turn at ledges (§5.2) while idling toward one.
            if body.ai.passive && at_ledge(world, body, size) {
                body.ai.dir = -body.ai.dir;
            }
        } else {
            // Hop.
            body.ai.counter += 1;
            let high = body.ai.counter.is_multiple_of(ed::SLIME_HIGH_HOP_EVERY);
            let vy = if high {
                ed::SLIME_HIGH_HOP_VY
            } else {
                ed::SLIME_HOP_VY
            };
            let dir = match (body.ai.passive, target) {
                (false, Some(t)) => {
                    if t.0 >= body.center().0 {
                        1.0
                    } else {
                        -1.0
                    }
                }
                _ => body.ai.dir as f32,
            };
            body.vel.0 = ed::SLIME_HOP_VX * dir;
            body.vel.1 = -vy;
            body.ai.timer = rng.gen_range_u32(
                secs_ticks(ed::SLIME_IDLE_MIN_SECS)..secs_ticks(ed::SLIME_IDLE_MAX_SECS),
            );
        }
    }
    body.ai.on_ground = step.on_ground;
}

/// Buoyancy: slimes float on water (lava slimes on lava; §5.2). Rising out
/// of lava gets the 1.5× bounce.
fn apply_float(body: &mut Entity, step: &BodyStep, float_on: LiquidKind, kind: EnemyKind) {
    if step.submerged == Some(float_on) {
        body.vel.1 -= ed::SLIME_BUOYANCY_ACCEL * DT;
        let max_rise = if kind == EnemyKind::LavaSlime {
            ed::SLIME_FLOAT_MAX_RISE * ed::LAVA_SLIME_BOUNCE_MULT
        } else {
            ed::SLIME_FLOAT_MAX_RISE
        };
        if body.vel.1 < -max_rise {
            body.vel.1 = -max_rise;
        }
    }
}

/// Whether the cell ahead-and-below of the body's leading edge is a drop
/// (the §5.2 passive-slime ledge turn test).
fn at_ledge(world: &World, body: &Entity, size: (f32, f32)) -> bool {
    let ahead_x = if body.ai.dir > 0 {
        body.pos.0 + size.0 + 0.5
    } else {
        body.pos.0 - 0.5
    };
    let below_y = body.pos.1 + size.1 + 0.5;
    !world.is_solid(ahead_x.floor() as i32, below_y.floor() as i32)
}

/// Fighter (Zombie 3.2 t/s, Skeleton 3.8): walks at the target, jumps (vy
/// 21) when blocked on the ground, auto-steps 1-tile ledges. Fleeing
/// (dawn): walks *away* from the target instead.
pub(crate) fn step_fighter(
    world: &World,
    kind: EnemyKind,
    body: &mut Entity,
    target: Option<(f32, f32)>,
) {
    let size = body.size();
    let speed = kind.walk_speed();
    if let Some(t) = target {
        let toward = if t.0 >= body.center().0 { 1 } else { -1 };
        body.ai.dir = if body.ai.fleeing { -toward } else { toward };
    }
    // Steer toward the walk speed (instant accel is fine for v1 walkers),
    // but never fight fresh knockback: only re-assert walking speed when
    // slower than it.
    let want = speed * body.ai.dir as f32;
    if (body.vel.0 - want).abs() > speed || body.vel.0.signum() != want.signum() {
        // Knocked back / turned: decay toward the walk velocity.
        let max = ed::FIGHTER_RECOVERY_ACCEL * DT;
        body.vel.0 += (want - body.vel.0).clamp(-max, max);
    } else {
        body.vel.0 = want;
    }
    let step = step_enemy_body(world, &mut body.pos, &mut body.vel, size, DT, true);
    if step.blocked_x && step.on_ground {
        body.vel.1 = -ed::FIGHTER_JUMP_VY;
    }
    body.ai.on_ground = step.on_ground;
}

/// Flier-bouncer (Demon Eye): accelerates toward the target (18 t/s², max
/// 9.4 t/s, turn ≤ 90°/s); on tile collision reflects velocity and adds an
/// upward kick. Fleeing: accelerates straight up and away.
pub(crate) fn step_bouncer(world: &World, body: &mut Entity, target: Option<(f32, f32)>) {
    let size = body.size();
    let center = body.center();
    let desired = match (body.ai.fleeing, target) {
        (true, Some(t)) => ((center.0 - t.0).signum(), -1.0),
        (false, Some(t)) => norm((t.0 - center.0, t.1 - center.1)),
        _ => (0.0, 0.0),
    };
    if desired != (0.0, 0.0) {
        let speed = len(body.vel);
        if speed > ed::BOUNCER_MIN_STEER_SPEED {
            // Turn-rate-limited steering: rotate the velocity toward the
            // desired heading at ≤ BOUNCER_TURN_RATE_DEG °/s, accelerating
            // along the (rotated) heading.
            let max_turn = ed::BOUNCER_TURN_RATE_DEG.to_radians() * DT;
            let cur = body.vel.1.atan2(body.vel.0);
            let want = desired.1.atan2(desired.0);
            let mut diff = want - cur;
            while diff > std::f32::consts::PI {
                diff -= std::f32::consts::TAU;
            }
            while diff < -std::f32::consts::PI {
                diff += std::f32::consts::TAU;
            }
            let new = cur + diff.clamp(-max_turn, max_turn);
            let new_speed = (speed + ed::BOUNCER_ACCEL * DT).min(ed::BOUNCER_MAX_SPEED);
            body.vel = (new.cos() * new_speed, new.sin() * new_speed);
        } else {
            body.vel.0 += desired.0 * ed::BOUNCER_ACCEL * DT;
            body.vel.1 += desired.1 * ed::BOUNCER_ACCEL * DT;
        }
    }
    let pre = body.vel;
    let step = step_flier_body(world, &mut body.pos, &mut body.vel, size, DT);
    if step.blocked_x {
        body.vel.0 = -pre.0;
    }
    if step.on_ground || step.hit_ceiling {
        body.vel.1 = -pre.1;
    }
    if step.blocked_x || step.on_ground || step.hit_ceiling {
        body.vel.1 -= ed::BOUNCER_BOUNCE_UP; // §5.2: bounce up
    }
}

/// Flier-erratic (Cave Bat): seeks at ≤12 t/s; every 0.25–0.6 s adds up to
/// ±6 t/s of jitter per axis.
pub(crate) fn step_erratic(
    world: &World,
    body: &mut Entity,
    target: Option<(f32, f32)>,
    rng: &mut ferraria_shared::rng::Pcg32,
) {
    let size = body.size();
    let center = body.center();
    if let Some(t) = target {
        let d = norm((t.0 - center.0, t.1 - center.1));
        let dir = if body.ai.fleeing { -1.0 } else { 1.0 };
        body.vel.0 += d.0 * dir * ed::ERRATIC_ACCEL * DT;
        body.vel.1 += d.1 * dir * ed::ERRATIC_ACCEL * DT;
    }
    if body.ai.timer == 0 {
        body.vel.0 += rng.gen_range_f32(-ed::ERRATIC_JITTER_SPEED, ed::ERRATIC_JITTER_SPEED);
        body.vel.1 += rng.gen_range_f32(-ed::ERRATIC_JITTER_SPEED, ed::ERRATIC_JITTER_SPEED);
        body.ai.timer = rng.gen_range_u32(
            secs_ticks(ed::ERRATIC_JITTER_MIN_SECS)..secs_ticks(ed::ERRATIC_JITTER_MAX_SECS),
        );
    } else {
        body.ai.timer -= 1;
    }
    let speed = len(body.vel);
    if speed > ed::ERRATIC_MAX_SPEED {
        let s = ed::ERRATIC_MAX_SPEED / speed;
        body.vel.0 *= s;
        body.vel.1 *= s;
    }
    let pre = body.vel;
    let step = step_flier_body(world, &mut body.pos, &mut body.vel, size, DT);
    // Bats slide along tiles rather than sticking to them.
    if step.blocked_x {
        body.vel.0 = -pre.0 * ed::ERRATIC_BOUNCE_DAMPING;
    }
    if step.on_ground || step.hit_ceiling {
        body.vel.1 = -pre.1 * ed::ERRATIC_BOUNCE_DAMPING;
    }
}

/// Watchling: no jitter — straight at the player at 10.5 t/s, blocked by
/// tiles normally (§5.2). Steers *toward* the chase velocity instead of
/// overwriting it each tick, so knockback actually displaces it (§5.1:
/// 0% KB resist, not immunity).
pub(crate) fn step_straight(world: &World, body: &mut Entity, target: Option<(f32, f32)>) {
    let size = body.size();
    let center = body.center();
    if let Some(t) = target {
        let d = norm((t.0 - center.0, t.1 - center.1));
        let want = (d.0 * ed::WATCHLING_SPEED, d.1 * ed::WATCHLING_SPEED);
        let max = ed::WATCHLING_STEER_ACCEL * DT;
        body.vel.0 += (want.0 - body.vel.0).clamp(-max, max);
        body.vel.1 += (want.1 - body.vel.1).clamp(-max, max);
    }
    step_flier_body(world, &mut body.pos, &mut body.vel, size, DT);
}

/// Swooper + caster (Ash Demon): hovers 8–12 tiles out, swoops through at
/// 14 t/s on a cadence, and — every 4 s with line of sight — returns
/// `Some(target)` to tell the sim to fire the 4-sickle volley.
pub(crate) fn step_swooper(
    world: &World,
    body: &mut Entity,
    target: Option<(f32, f32)>,
    rng: &mut ferraria_shared::rng::Pcg32,
) -> Option<(f32, f32)> {
    let size = body.size();
    let center = body.center();
    let mut volley = None;
    if let Some(t) = target {
        match body.ai.phase {
            // Hover: steer toward the hover ring around the player.
            0 => {
                let to = (t.0 - center.0, t.1 - center.1);
                let dist = len(to).max(1e-3);
                let mid = (ed::SWOOPER_HOVER_MIN + ed::SWOOPER_HOVER_MAX) * 0.5;
                // Radial correction toward the ring + slight upward bias so
                // it hovers above ground clutter.
                let radial = if dist > ed::SWOOPER_HOVER_MAX {
                    1.0
                } else if dist < ed::SWOOPER_HOVER_MIN {
                    -1.0
                } else {
                    (dist - mid) / mid * ed::SWOOPER_RING_GAIN
                };
                let d = (to.0 / dist, to.1 / dist);
                body.vel.0 += d.0 * radial * ed::SWOOPER_HOVER_ACCEL * DT;
                body.vel.1 +=
                    (d.1 * radial - ed::SWOOPER_UPWARD_BIAS) * ed::SWOOPER_HOVER_ACCEL * DT;
                let speed = len(body.vel);
                if speed > ed::SWOOPER_HOVER_MAX_SPEED {
                    let s = ed::SWOOPER_HOVER_MAX_SPEED / speed;
                    body.vel.0 *= s;
                    body.vel.1 *= s;
                }
                if body.ai.timer == 0 {
                    // Begin a swoop straight through the player.
                    body.ai.phase = 1;
                    body.ai.timer = secs_ticks(ed::SWOOPER_SWOOP_SECS);
                    let d = (to.0 / dist, to.1 / dist);
                    body.vel = (d.0 * ed::SWOOPER_SWOOP_SPEED, d.1 * ed::SWOOPER_SWOOP_SPEED);
                } else {
                    body.ai.timer -= 1;
                }
            }
            // Swoop: keep the velocity, count down, retreat to hover.
            _ => {
                if body.ai.timer == 0 {
                    body.ai.phase = 0;
                    body.ai.timer = secs_ticks(ed::SWOOPER_SWOOP_PERIOD_SECS);
                } else {
                    body.ai.timer -= 1;
                }
            }
        }
        // Volley every 4 s of line of sight (§5.2).
        if body.ai.timer2 == 0 {
            if ferraria_shared::physics::line_of_sight(world, center, t) {
                volley = Some(t);
                body.ai.timer2 = secs_ticks(ed::SWOOPER_VOLLEY_PERIOD_SECS);
            }
        } else {
            body.ai.timer2 -= 1;
        }
        let _ = rng;
    }
    step_flier_body(world, &mut body.pos, &mut body.vel, size, DT);
    volley
}

fn len(v: (f32, f32)) -> f32 {
    (v.0 * v.0 + v.1 * v.1).sqrt()
}

fn norm(v: (f32, f32)) -> (f32, f32) {
    let l = len(v);
    if l <= 1e-6 {
        (0.0, 0.0)
    } else {
        (v.0 / l, v.1 / l)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;
    use ferraria_shared::protocol::{EntityKind as WireKind, ServerMessage};
    use ferraria_shared::rng::Pcg32;
    use ferraria_shared::tiles::{Tile, TileId};
    use ferraria_shared::world::NEW_WORLD_TIME;

    /// World from ASCII art: `#` stone, `.` air.
    fn ascii_world(rows: &[&str]) -> World {
        let mut w = World::new(rows[0].len() as u32, rows.len() as u32);
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                if ch == '#' {
                    w.set_tile(x as u32, y as u32, Tile::of(TileId::Stone));
                }
            }
        }
        w
    }

    /// A wide flat world: all air above `floor`, stone below.
    fn flat_world(width: u32, height: u32, floor: u32) -> World {
        let mut w = World::new(width, height);
        for y in floor..height {
            for x in 0..width {
                w.set_tile(x, y, Tile::of(TileId::Stone));
            }
        }
        w
    }

    fn enemy_on_floor(kind: EnemyKind, x: f32, floor: u32) -> Entity {
        let (w, h) = kind.data().size;
        Entity {
            pos: (x - w / 2.0, floor as f32 - h - 1e-3),
            vel: (0.0, 0.0),
            kind: EntityKind::Enemy(kind),
            spawn_tick: 0,
            awake: true,
            hp: kind.data().max_hp,
            hp_dirty: false,
            ai: AiState {
                timer: 60,
                dir: 1,
                ..AiState::default()
            },
        }
    }

    // ---- §5.2 golden AI tests ------------------------------------------------

    #[test]
    fn slime_hop_cadence_and_third_high_hop() {
        let world = flat_world(200, 60, 40);
        let mut rng = Pcg32::new(7);
        let mut slime = enemy_on_floor(EnemyKind::GreenSlime, 50.0, 40);
        let target = Some((120.0, 38.0)); // hostile, target to the right

        // Record (tick, vy) at each hop launch.
        let mut hops: Vec<(u32, f32, f32)> = Vec::new();
        let mut prev_vy = 0.0f32;
        for tick in 0..60 * 30 {
            step_slime(&world, EnemyKind::GreenSlime, &mut slime, target, &mut rng);
            if slime.vel.1 < -15.0 && prev_vy >= -1.0 {
                hops.push((tick, slime.vel.0, slime.vel.1));
            }
            prev_vy = slime.vel.1;
        }
        assert!(hops.len() >= 6, "kept hopping ({} hops)", hops.len());
        for (i, &(_, vx, vy)) in hops.iter().enumerate() {
            // §5.2: vx 5.6 toward the player (target is to the right).
            assert!((vx - ed::SLIME_HOP_VX).abs() < 1e-3, "hop {i} vx {vx}");
            let expect = if (i + 1) % ed::SLIME_HIGH_HOP_EVERY as usize == 0 {
                ed::SLIME_HIGH_HOP_VY // every 3rd hop is high
            } else {
                ed::SLIME_HOP_VY
            };
            assert!(
                (vy + expect).abs() < 1e-3,
                "hop {i} vy {vy}, want -{expect}"
            );
        }
        // Idle gap between hops: 0.7–2.0 s plus the ~28-tick flight.
        for pair in hops.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!(
                (42 + 20..=120 + 40).contains(&gap),
                "hop gap {gap} ticks out of the §5.2 cadence band"
            );
        }
    }

    #[test]
    fn fighter_clears_two_tile_walls_but_not_three() {
        // Floor top y=20, wall of stone at x=30 of the given height.
        let walled = |wall_h: u32| -> World {
            let mut w = flat_world(100, 40, 20);
            for dy in 1..=wall_h {
                w.set_tile(30, 20 - dy, Tile::of(TileId::Stone));
            }
            w
        };
        let run = |world: &World, ticks: u32| -> f32 {
            let mut z = enemy_on_floor(EnemyKind::Zombie, 24.0, 20);
            let target = Some((60.0, 18.0));
            for _ in 0..ticks {
                step_fighter(world, EnemyKind::Zombie, &mut z, target);
            }
            z.center().0
        };
        // 2-tile wall: §5.2 jump (vy 21 ≈ 2.45 tile apex) clears it.
        assert!(
            run(&walled(2), 1200) > 32.0,
            "zombie failed to clear a 2-tile wall"
        );
        // 3-tile wall: stuck on the near side forever.
        assert!(
            run(&walled(3), 1800) < 30.0,
            "zombie cleared a 3-tile wall (apex should be ~2.45 tiles)"
        );
        // 1-tile ledge: auto-step, no jump needed (§5.2).
        let mut stepped = flat_world(100, 40, 20);
        for x in 30..100 {
            stepped.set_tile(x, 19, Tile::of(TileId::Stone));
        }
        let mut z = enemy_on_floor(EnemyKind::Zombie, 24.0, 20);
        for _ in 0..600 {
            step_fighter(&stepped, EnemyKind::Zombie, &mut z, Some((60.0, 17.0)));
        }
        assert!(z.center().0 > 35.0, "auto-stepped the 1-tile ledge");
    }

    #[test]
    fn bouncer_reflects_and_kicks_up_on_walls() {
        // Box with a wall at x=20; eye flies right at a target behind it.
        let mut world = flat_world(60, 40, 35);
        for y in 0..35 {
            world.set_tile(20, y, Tile::of(TileId::Stone));
        }
        let mut eye = enemy_on_floor(EnemyKind::DemonEye, 10.0, 20);
        eye.pos.1 = 15.0;
        eye.vel = (8.0, 0.0);
        let target = Some((40.0, 15.0));
        let mut bounced = None;
        for _ in 0..240 {
            let before_vx = eye.vel.0;
            step_bouncer(&world, &mut eye, target);
            if before_vx > 0.0 && eye.vel.0 < 0.0 {
                bounced = Some(eye.vel);
                break;
            }
        }
        let (vx, vy) = bounced.expect("hit the wall and reflected");
        assert!(vx < 0.0, "horizontal velocity reflected");
        assert!(
            vy <= -ed::BOUNCER_BOUNCE_UP + 0.5,
            "upward kick applied (vy {vy})"
        );
    }

    #[test]
    fn watchling_knockback_decays_instead_of_being_overwritten() {
        let world = flat_world(200, 100, 90);
        let mut w = enemy_on_floor(EnemyKind::Watchling, 50.0, 40);
        w.pos.1 = 30.0; // airborne
        let target = Some((80.0, 32.0));
        // Settle into the straight chase first.
        for _ in 0..60 {
            step_straight(&world, &mut w, target);
        }
        assert!(w.vel.0 > ed::WATCHLING_SPEED * 0.8, "chasing right");
        // Fresh knockback against the chase direction (§5.1: 0% resist, so
        // the full impulse applies) must not be erased on the next tick.
        w.vel = (-8.0, -4.0);
        step_straight(&world, &mut w, target);
        assert!(
            w.vel.0 < -6.0,
            "one tick must not erase knockback (vx {})",
            w.vel.0
        );
        // ...but it steers back to the §5.2 chase velocity within ~a second.
        for _ in 0..90 {
            step_straight(&world, &mut w, target);
        }
        assert!(
            w.vel.0 > ed::WATCHLING_SPEED * 0.7,
            "recovered the chase (vx {})",
            w.vel.0
        );
    }

    // ---- §5.1 slime passivity ---------------------------------------------------

    #[test]
    fn slime_passivity_follows_damage_depth_and_daylight() {
        let surface = (ed::UNDERGROUND_START_ROW - 1) as f32;
        assert!(slime_passive(false, surface, true), "day surface: passive");
        assert!(!slime_passive(false, surface, false), "night: hostile");
        assert!(
            !slime_passive(false, ed::UNDERGROUND_START_ROW as f32, true),
            "underground: hostile even by day"
        );
        assert!(!slime_passive(true, surface, true), "damaged: hostile");
    }

    #[test]
    fn dusk_angers_passive_slimes_and_dawn_calms_only_the_undamaged() {
        let mut sim = flat_sim(100, 60, 30);
        assert!(sim.world.is_day());
        let a = sim.spawn_enemy(
            EnemyKind::GreenSlime,
            (40.0, 28.0),
            SpawnEnvironment::SurfaceDay,
        );
        let b = sim.spawn_enemy(
            EnemyKind::BlueSlime,
            (60.0, 28.0),
            SpawnEnvironment::SurfaceDay,
        );
        advance(&mut sim, 1);
        assert!(sim.entities.map[&a].ai.passive && sim.entities.map[&b].ai.passive);

        // Dusk: both turn hostile without ever being touched (§5.1 "always
        // hostile at night").
        sim.world.time = 0;
        advance(&mut sim, 1);
        assert!(!sim.entities.map[&a].ai.passive, "hostile at night");
        assert!(!sim.entities.map[&b].ai.passive, "hostile at night");

        // Damage one during the night; at dawn only the untouched slime
        // re-passivates ("until damaged" is the one permanent aggro).
        sim.hurt_enemy(
            a,
            1,
            super::super::combat::DamageSource::Player(1),
            0.0,
            1.0,
            None,
        );
        sim.world.time = NEW_WORLD_TIME;
        advance(&mut sim, 1);
        assert!(
            !sim.entities.map[&a].ai.passive,
            "damage aggro is permanent"
        );
        assert!(sim.entities.map[&b].ai.passive, "undamaged: docile by day");
    }

    // ---- §5.3 placement & spawn-algorithm tests --------------------------------

    #[test]
    fn placement_rules_ground_vs_air() {
        let world = ascii_world(&[
            "..........", // 0
            "..........", // 1
            "..........", // 2
            "....#.....", // 3  lone block at (4,3)
            "..........", // 4
            "##########", // 5 floor
        ]);
        // Grounded: floor tile with 3×2 clear air above it.
        assert!(placement_for(&world, EnemyKind::GreenSlime, 7, 5).is_some());
        // Grounded on the floor under the lone block: air above (4,5) is
        // blocked at (4,3)? No — only rows y-1, y-2 matter: (4,4) and (4,3);
        // (4,3) is solid → refused.
        assert!(placement_for(&world, EnemyKind::GreenSlime, 4, 5).is_none());
        // Not solid → refused.
        assert!(placement_for(&world, EnemyKind::GreenSlime, 2, 2).is_none());
        // Flier: a 2×2 air pocket anywhere works...
        assert!(placement_for(&world, EnemyKind::CaveBat, 1, 1).is_some());
        // ...but not overlapping the lone block.
        assert!(placement_for(&world, EnemyKind::CaveBat, 3, 2).is_none());
        assert!(placement_for(&world, EnemyKind::CaveBat, 4, 3).is_none());
        // Grounded placement stands on top of the tile.
        let (_, h) = EnemyKind::GreenSlime.data().size;
        let pos = placement_for(&world, EnemyKind::GreenSlime, 7, 5).expect("ground spot");
        assert!((pos.1 + h - 5.0).abs() < 0.01, "feet on the floor top");
    }

    #[test]
    fn surface_day_spawns_slimes_in_the_ring_up_to_cap() {
        let mut sim = flat_sim(300, 200, 150);
        assert_eq!(sim.world.time, NEW_WORLD_TIME, "day");
        let (id, _epoch, mut rx) = join(&mut sim, "alice");
        place_player(&mut sim, id, 150.0, 150.0);
        drain(&mut rx);
        let player = sim.players[&id].center();

        advance(&mut sim, 6000);
        let spawns: Vec<(WireKind, (f32, f32))> = drain(&mut rx)
            .into_iter()
            .filter_map(|m| match m {
                ServerMessage::EntitySpawn { kind, pos, .. } => Some((kind, pos)),
                _ => None,
            })
            .collect();
        assert!(!spawns.is_empty(), "surface-day spawns happened");
        let (_, m) = SpawnEnvironment::SurfaceDay.spawn_params();
        assert!(
            sim.live_enemies() <= m,
            "crowding capped at M={m}, got {}",
            sim.live_enemies()
        );
        for (kind, pos) in &spawns {
            // §5.3 step 5: surface day table is green/blue slimes only.
            assert!(
                matches!(kind, WireKind::GreenSlime | WireKind::BlueSlime),
                "unexpected day species {kind:?}"
            );
            // §5.3 step 4: in the ring, outside the screen rect — measured
            // at the enemy's center (what placement validated; the AABB
            // top-left sits up to half a body inside it).
            let (w, h) = EnemyKind::from_wire(*kind).expect("enemy kind").data().size;
            let c = (pos.0 + w / 2.0, pos.1 + h / 2.0);
            let (dx, dy) = (c.0 - player.0, c.1 - player.1);
            assert!(
                !ed::in_spawn_safe_rect(dx, dy),
                "spawned on-screen at {pos:?}"
            );
            assert!(
                dx.abs() <= ed::SPAWN_RING_OUTER_X as f32 + 2.0
                    && dy.abs() <= ed::SPAWN_RING_OUTER_Y as f32 + 2.0,
                "outside the outer ring: {pos:?}"
            );
        }
        // Day-surface slimes are passive (§5.1).
        assert!(sim
            .entities
            .map
            .values()
            .filter(|e| matches!(e.kind, EntityKind::Enemy(_)))
            .all(|e| e.ai.passive));
    }

    #[test]
    fn night_uses_the_night_table() {
        let mut sim = flat_sim(300, 200, 150);
        sim.world.time = 0; // midnight
        let (id, _epoch, mut rx) = join(&mut sim, "alice");
        place_player(&mut sim, id, 150.0, 150.0);
        drain(&mut rx);
        advance(&mut sim, 4000);
        let kinds: Vec<WireKind> = drain(&mut rx)
            .into_iter()
            .filter_map(|m| match m {
                ServerMessage::EntitySpawn { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert!(!kinds.is_empty(), "night spawns happened");
        for kind in kinds {
            assert!(
                matches!(
                    kind,
                    WireKind::Zombie | WireKind::DemonEye | WireKind::BlueSlime
                ),
                "unexpected night species {kind:?}"
            );
        }
    }

    #[test]
    fn crowding_at_cap_blocks_all_spawns() {
        let mut sim = flat_sim(300, 200, 150);
        let (id, _epoch, mut rx) = join(&mut sim, "alice");
        place_player(&mut sim, id, 150.0, 150.0);
        let (_, m) = SpawnEnvironment::SurfaceDay.spawn_params();
        for i in 0..m {
            sim.spawn_enemy(
                EnemyKind::GreenSlime,
                (140.0 + i as f32 * 4.0, 145.0),
                SpawnEnvironment::SurfaceDay,
            );
        }
        drain(&mut rx);
        advance(&mut sim, 3000);
        let new_spawns = drain(&mut rx)
            .iter()
            .filter(|msg| matches!(msg, ServerMessage::EntitySpawn { .. }))
            .count();
        assert_eq!(new_spawns, 0, "C ≥ M must block spawning");
        assert_eq!(sim.live_enemies(), m);
    }

    #[test]
    fn enemies_despawn_outside_all_player_rectangles() {
        let mut sim = flat_sim(600, 200, 150);
        sim.world.time = 0; // night — zombies must not dawn-flee mid-test
        let (id, _epoch, mut rx) = join(&mut sim, "alice");
        place_player(&mut sim, id, 100.0, 150.0);
        // One enemy just inside the rect, one outside (dx > 168).
        let near = sim.spawn_enemy(
            EnemyKind::Zombie,
            (220.0, 145.0),
            SpawnEnvironment::SurfaceNight,
        );
        let far = sim.spawn_enemy(
            EnemyKind::Zombie,
            (350.0, 145.0),
            SpawnEnvironment::SurfaceNight,
        );
        drain(&mut rx);
        advance(&mut sim, DESPAWN_SWEEP_TICKS as u32 + 2);
        assert!(sim.entities.map.contains_key(&near), "in range: kept");
        assert!(!sim.entities.map.contains_key(&far), "out of range: gone");
        assert!(drain(&mut rx).iter().any(|m| matches!(m,
            ServerMessage::EntityDespawn { id, reason: DespawnReason::Despawned } if *id == far)));
    }

    // ---- §5.1 drop tables -------------------------------------------------------

    #[test]
    fn zombie_drop_table_distribution() {
        let mut sim = flat_sim(100, 60, 30);
        sim.loot_rng = Pcg32::new(42);
        let trials = 2000u32;
        let (mut wood_drops, mut arm_drops) = (0u32, 0u32);
        let (mut coin_min, mut coin_max) = (u32::MAX, 0u32);
        for _ in 0..trials {
            let id = sim
                .entities
                .insert(enemy_on_floor(EnemyKind::Zombie, 50.0, 30));
            sim.kill_enemy(id);
            let mut coins = 0u32;
            for e in sim.entities.map.values() {
                let EntityKind::ItemDrop { item, count } = e.kind else {
                    continue;
                };
                match item {
                    ferraria_shared::items::ItemId::Wood => wood_drops += 1,
                    ferraria_shared::items::ItemId::ZombieArm => arm_drops += 1,
                    ferraria_shared::items::ItemId::CopperCoin => coins += count as u32,
                    ferraria_shared::items::ItemId::SilverCoin => coins += count as u32 * 100,
                    _ => {}
                }
            }
            coin_min = coin_min.min(coins);
            coin_max = coin_max.max(coins);
            sim.entities.map.clear();
        }
        // §5.1: 50% 1 Wood, 2% Zombie Arm, 60 CC ± 20%.
        let wood_pct = wood_drops as f32 / trials as f32;
        let arm_pct = arm_drops as f32 / trials as f32;
        assert!((0.46..0.54).contains(&wood_pct), "wood rate {wood_pct}");
        assert!((0.008..0.035).contains(&arm_pct), "arm rate {arm_pct}");
        assert!(
            coin_min >= 48 && coin_max <= 72,
            "coins {coin_min}..{coin_max}"
        );
    }

    #[test]
    fn slime_drops_gel_always_lava_slime_never() {
        let mut sim = flat_sim(100, 60, 30);
        sim.loot_rng = Pcg32::new(9);
        for _ in 0..200 {
            let id = sim
                .entities
                .insert(enemy_on_floor(EnemyKind::GreenSlime, 50.0, 30));
            sim.kill_enemy(id);
            let gel: u32 = sim
                .entities
                .map
                .values()
                .filter_map(|e| match e.kind {
                    EntityKind::ItemDrop {
                        item: ferraria_shared::items::ItemId::Gel,
                        count,
                    } => Some(count as u32),
                    _ => None,
                })
                .sum();
            assert!((1..=2).contains(&gel), "green slime gel {gel}");
            sim.entities.map.clear();
        }
        let id = sim
            .entities
            .insert(enemy_on_floor(EnemyKind::LavaSlime, 50.0, 30));
        sim.kill_enemy(id);
        assert!(
            !sim.entities.map.values().any(|e| matches!(
                e.kind,
                EntityKind::ItemDrop {
                    item: ferraria_shared::items::ItemId::Gel,
                    ..
                }
            )),
            "lava slimes drop no gel (§5.1)"
        );
    }
}
