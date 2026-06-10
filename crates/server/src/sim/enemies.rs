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
            let (cx, cy) = (
                center.0 as i64 + dx as i64,
                center.1 as i64 + dy as i64,
            );
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
                timer: self
                    .spawn_rng
                    .gen_range_u32(secs_ticks(ed::SLIME_IDLE_MIN_SECS)..secs_ticks(ed::SLIME_IDLE_MAX_SECS)),
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
                    .gen_range_u32(row.min as u32..row.max as u32 + 1) as u16;
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
        body.vel.0 *= 0.8;
        if body.vel.0.abs() < 0.1 {
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
        body.vel.0 += (want - body.vel.0).clamp(-30.0 * DT, 30.0 * DT);
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
        if speed > 0.5 {
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
        body.vel.0 = -pre.0 * 0.5;
    }
    if step.on_ground || step.hit_ceiling {
        body.vel.1 = -pre.1 * 0.5;
    }
}

/// Watchling: no jitter — straight at the player at 10.5 t/s, blocked by
/// tiles normally (§5.2).
pub(crate) fn step_straight(world: &World, body: &mut Entity, target: Option<(f32, f32)>) {
    let size = body.size();
    let center = body.center();
    if let Some(t) = target {
        let d = norm((t.0 - center.0, t.1 - center.1));
        body.vel = (d.0 * ed::WATCHLING_SPEED, d.1 * ed::WATCHLING_SPEED);
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
                    (dist - mid) / mid * 0.4
                };
                let d = (to.0 / dist, to.1 / dist);
                body.vel.0 += d.0 * radial * ed::SWOOPER_HOVER_ACCEL * DT;
                body.vel.1 += (d.1 * radial - 0.2) * ed::SWOOPER_HOVER_ACCEL * DT;
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
