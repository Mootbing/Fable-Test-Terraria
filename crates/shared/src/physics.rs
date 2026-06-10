//! Pure player movement: AABB vs. tile grid (DESIGN §8, fluids §3).
//!
//! Runs identically on the client (prediction for the own player) and the
//! server (sanity checks); it must stay deterministic and free of I/O.
//!
//! Conventions: positions are the **top-left corner** of the AABB in tile
//! units, `y` grows downward, velocities in tiles/second.

use serde::{Deserialize, Serialize};

use crate::tiles::{LiquidKind, TileId};
use crate::world::World;

// ---- Movement constants (§0 and §8) ----------------------------------------

/// Gravity, tiles/s² (caps all entities).
pub const GRAVITY: f32 = 90.0;
/// Terminal fall velocity, tiles/s (caps all entities).
pub const TERMINAL_VELOCITY: f32 = 37.5;
/// Max run speed, tiles/s.
pub const RUN_MAX_SPEED: f32 = 11.25;
/// Run acceleration, tiles/s².
pub const RUN_ACCEL: f32 = 18.0;
/// Ground friction deceleration with no input, tiles/s².
pub const GROUND_FRICTION: f32 = 45.0;
/// Hold-to-rise jump: vy held at −18.79 t/s for up to 0.25 s, then ballistic.
/// Full hold tops out around 6.5 tiles.
pub const JUMP_SPEED: f32 = 18.79;
pub const JUMP_HOLD_SECS: f32 = 0.25;
/// 1-tile ledges are stepped up automatically while grounded.
pub const AUTO_STEP_TILES: f32 = 1.0;

pub const PLAYER_WIDTH: f32 = 1.25;
pub const PLAYER_HEIGHT: f32 = 2.75;

/// Liquid modifiers (§3): apply while the body center is submerged.
pub const LIQUID_SPEED_MULT: f32 = 0.5;
pub const LIQUID_GRAVITY_MULT: f32 = 0.4;
pub const LIQUID_TERMINAL_MULT: f32 = 0.5;
/// In liquid, jump becomes a repeatable swim impulse.
pub const SWIM_IMPULSE: f32 = 12.0;

/// Cobweb (§2 tile 30): entities inside have velocity clamped to 1.5 t/s.
pub const COBWEB_MAX_SPEED: f32 = 1.5;

/// Fall damage (§8): safe up to 25 tiles, then 10 dmg per extra tile.
pub const SAFE_FALL_TILES: f32 = 25.0;
pub const FALL_DAMAGE_PER_TILE: f32 = 10.0;

// ---- Item-drop entity tuning -------------------------------------------------
// Gravity/terminal velocity are the global §0 caps above; DESIGN doesn't pin
// the rest, so it's canonized here (like the enemy hitboxes below).

/// Ground friction deceleration for resting item drops, tiles/s².
pub const ITEM_FRICTION: f32 = 30.0;
/// Fresh drops pop out with vx in ±this, tiles/s...
pub const ITEM_SPAWN_SPEED_X: f32 = 3.0;
/// ...and vy upward between these two magnitudes, tiles/s.
pub const ITEM_SPAWN_SPEED_Y: (f32, f32) = (2.0, 6.0);

/// How long platforms stay intangible after a Down+Jump drop.
pub const DROP_THROUGH_SECS: f32 = 0.25;

/// Collision skin: kept between the AABB and tile faces so flush contacts
/// stay numerically stable (must exceed one f32 ulp at x ≈ 4200).
pub const COLLISION_EPS: f32 = 1e-3;

/// Entity hitbox sizes `(w, h)` in tiles. The player and boss sizes are from
/// DESIGN (§8, §6); enemy sizes are not specified there and are canonized
/// here.
pub mod hitbox {
    pub const PLAYER: (f32, f32) = (super::PLAYER_WIDTH, super::PLAYER_HEIGHT);
    pub const GREEN_SLIME: (f32, f32) = (1.5, 1.0);
    pub const BLUE_SLIME: (f32, f32) = (1.75, 1.25);
    pub const ZOMBIE: (f32, f32) = (1.25, 2.75);
    pub const DEMON_EYE: (f32, f32) = (1.75, 1.75);
    pub const CAVE_BAT: (f32, f32) = (1.0, 1.0);
    pub const SKELETON: (f32, f32) = (1.25, 2.75);
    pub const LAVA_SLIME: (f32, f32) = (1.75, 1.25);
    pub const ASH_DEMON: (f32, f32) = (2.0, 3.0);
    pub const WATCHLING: (f32, f32) = (1.0, 1.0);
    /// §6.1: 6×4 at full HP (scales with HP).
    pub const SLIME_MONARCH: (f32, f32) = (6.0, 4.0);
    /// §6.2: 4×4 both phases.
    pub const WATCHER: (f32, f32) = (4.0, 4.0);
    pub const BONE_WARDEN_SKULL: (f32, f32) = (3.0, 3.5);
    pub const BONE_WARDEN_HAND: (f32, f32) = (2.0, 2.0);
    pub const ITEM_DROP: (f32, f32) = (0.75, 0.75);
    pub const ARROW: (f32, f32) = (0.5, 0.5);
    pub const VOID_SICKLE: (f32, f32) = (1.0, 1.0);
    /// A falling tile (§2 tile 4 sand) is exactly one cell.
    pub const FALLING_TILE: (f32, f32) = (1.0, 1.0);
}

/// Fall damage for a completed fall (§8). Mitigations (Lucky Charm, Gust Jar
/// mid-air jump) are applied by the server on top of this.
pub fn fall_damage(tiles_fallen: f32) -> u32 {
    if tiles_fallen <= SAFE_FALL_TILES {
        0
    } else {
        ((tiles_fallen - SAFE_FALL_TILES) * FALL_DAMAGE_PER_TILE) as u32
    }
}

/// Physics-affecting equipment modifiers (§4.3 accessories), produced from a
/// player's worn loadout by [`crate::loadout::physics_mods`]. Both the server
/// and the predicting client derive this from the same inventory through the
/// same pure function, so they can never disagree.
///
/// `Default` is "no accessories": multiplier 1, no extra jumps, fall damage
/// applies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysicsMods {
    /// Max-run-speed multiplier (Swift Boots: ×1.25).
    pub speed_mult: f32,
    /// Mid-air jumps available per airborne stretch (Gust Jar: 1).
    pub extra_air_jumps: u8,
    /// Lucky Charm: landing never deals fall damage.
    pub no_fall_damage: bool,
}

impl Default for PhysicsMods {
    fn default() -> Self {
        PhysicsMods::NONE
    }
}

impl PhysicsMods {
    /// Bare loadout: no speed boost, no double jump, fall damage applies.
    pub const NONE: PhysicsMods = PhysicsMods {
        speed_mult: 1.0,
        extra_air_jumps: 0,
        no_fall_damage: false,
    };
}

/// Mutable physics state for one player.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlayerPhysics {
    /// Top-left corner of the AABB, tile units.
    pub pos: (f32, f32),
    /// Tiles/second; +y is down.
    pub vel: (f32, f32),
    pub on_ground: bool,
    /// Seconds of hold-to-rise remaining (0 when not in the rise phase).
    pub jump_hold_left: f32,
    /// Jump key state last tick, for press-edge detection.
    pub jump_was_held: bool,
    /// Seconds platforms remain intangible after a Down+Jump drop.
    pub drop_through: f32,
    /// Tiles fallen since last grounded/swimming, for fall damage.
    pub fall_distance: f32,
    /// Run-speed multiplier (1.0 base; Swift Boots set 1.25).
    ///
    /// Superseded by [`PhysicsMods::speed_mult`] (the multipliers stack
    /// multiplicatively; loadout code uses mods and leaves this at 1.0).
    pub run_speed_mult: f32,
    /// Mid-air jumps spent since last grounded/swimming (Gust Jar, §4.3).
    pub air_jumps_used: u8,
}

impl PlayerPhysics {
    /// At rest with the AABB's top-left at `pos`.
    pub fn new(pos: (f32, f32)) -> PlayerPhysics {
        PlayerPhysics {
            pos,
            vel: (0.0, 0.0),
            on_ground: false,
            jump_hold_left: 0.0,
            jump_was_held: false,
            drop_through: 0.0,
            fall_distance: 0.0,
            run_speed_mult: 1.0,
            air_jumps_used: 0,
        }
    }

    /// At rest with the feet center at (`x_center`, `y_feet`) — e.g. standing
    /// on top of the tile row starting at `y_feet`.
    pub fn from_feet(x_center: f32, y_feet: f32) -> PlayerPhysics {
        PlayerPhysics::new((
            x_center - PLAYER_WIDTH / 2.0,
            y_feet - PLAYER_HEIGHT - COLLISION_EPS,
        ))
    }

    pub fn center(&self) -> (f32, f32) {
        (
            self.pos.0 + PLAYER_WIDTH / 2.0,
            self.pos.1 + PLAYER_HEIGHT / 2.0,
        )
    }

    pub fn feet_y(&self) -> f32 {
        self.pos.1 + PLAYER_HEIGHT
    }
}

/// Player intent for one tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayerInput {
    pub left: bool,
    pub right: bool,
    pub jump: bool,
    pub down: bool,
}

/// What happened during one step, for the caller (fall damage, breath,
/// burning, sounds).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StepResult {
    /// Hit the ground this tick after falling.
    pub landed: bool,
    /// Tiles fallen when landing (0 unless `landed`). Feed to
    /// [`fall_damage`].
    pub fall_distance: f32,
    /// AABB touches a water cell.
    pub in_water: bool,
    /// AABB touches a lava cell (server applies contact damage).
    pub in_lava: bool,
    /// Body center submerged in any liquid — the same test that switches
    /// the step to swim physics (§3), and what the swim animation
    /// ([`crate::protocol::anim::IN_LIQUID`]) keys off. Ankle-deep wading
    /// sets `in_water` but not this.
    pub swimming: bool,
    pub in_cobweb: bool,
    pub hit_ceiling: bool,
}

/// Hold-to-rise window for a Gust Jar mid-air jump, derived so its apex is
/// exactly [`crate::items::GUST_JAR_SECOND_JUMP_MULT`] (75%) of a full-hold
/// ground jump's. A full jump rises `J·T` during the hold plus the ballistic
/// `J²/2g` after it; solving `J·T' + J²/2g = mult · (J·T + J²/2g)` for the
/// air-jump hold `T'` is linear:
pub fn air_jump_hold_secs() -> f32 {
    let ballistic = JUMP_SPEED * JUMP_SPEED / (2.0 * GRAVITY);
    let full_height = JUMP_SPEED * JUMP_HOLD_SECS + ballistic;
    let target = full_height * crate::items::GUST_JAR_SECOND_JUMP_MULT;
    ((target - ballistic) / JUMP_SPEED).max(0.0)
}

#[inline]
fn cell(v: f32) -> i32 {
    v.floor() as i32
}

/// Tile rows/cols overlapped by an AABB (inclusive ranges).
fn cells(pos: (f32, f32), size: (f32, f32)) -> (i32, i32, i32, i32) {
    let c0 = cell(pos.0 + COLLISION_EPS);
    let c1 = cell(pos.0 + size.0 - COLLISION_EPS);
    let r0 = cell(pos.1 + COLLISION_EPS);
    let r1 = cell(pos.1 + size.1 - COLLISION_EPS);
    (c0, c1, r0, r1)
}

fn aabb_overlaps_solid(world: &World, pos: (f32, f32), size: (f32, f32)) -> bool {
    let (c0, c1, r0, r1) = cells(pos, size);
    (r0..=r1).any(|r| (c0..=c1).any(|c| world.is_solid(c, r)))
}

fn aabb_overlaps_tile(world: &World, pos: (f32, f32), size: (f32, f32), id: TileId) -> bool {
    let (c0, c1, r0, r1) = cells(pos, size);
    (r0..=r1)
        .any(|r| (c0..=c1).any(|c| c >= 0 && r >= 0 && world.tile(c as u32, r as u32).id == id))
}

fn aabb_touches_liquid(world: &World, pos: (f32, f32), size: (f32, f32), kind: LiquidKind) -> bool {
    let (c0, c1, r0, r1) = cells(pos, size);
    (r0..=r1).any(|r| (c0..=c1).any(|c| world.liquid(c, r).kind() == Some(kind)))
}

/// Liquid at the body center — the "submerged" test driving swim physics.
fn liquid_at_center(world: &World, pos: (f32, f32), size: (f32, f32)) -> Option<LiquidKind> {
    world
        .liquid(cell(pos.0 + size.0 / 2.0), cell(pos.1 + size.1 / 2.0))
        .kind()
}

fn column_solid(world: &World, c: i32, r0: i32, r1: i32) -> bool {
    (r0..=r1).any(|r| world.is_solid(c, r))
}

/// Sweeps the AABB horizontally by `dx` against solid tiles. Returns the new
/// x and whether the move was blocked.
fn sweep_x(world: &World, pos: (f32, f32), size: (f32, f32), dx: f32) -> (f32, bool) {
    let (x, y) = pos;
    let (w, h) = size;
    let r0 = cell(y + COLLISION_EPS);
    let r1 = cell(y + h - COLLISION_EPS);
    if dx > 0.0 {
        let old_edge = x + w;
        let desired = old_edge + dx;
        for c in cell(old_edge + COLLISION_EPS)..=cell(desired) {
            if column_solid(world, c, r0, r1) {
                return (c as f32 - COLLISION_EPS - w, true);
            }
        }
        (desired - w, false)
    } else if dx < 0.0 {
        let old_edge = x;
        let desired = old_edge + dx;
        let c_start = cell(old_edge - COLLISION_EPS);
        let c_end = cell(desired);
        for c in (c_end..=c_start).rev() {
            if column_solid(world, c, r0, r1) {
                return ((c + 1) as f32 + COLLISION_EPS, true);
            }
        }
        (desired, false)
    } else {
        (x, false)
    }
}

/// Sweeps the AABB vertically by `dy`. Falling collides with solids and (top
/// faces of) platforms; rising only with solids. Returns
/// `(new_y, hit_floor, hit_ceiling)`.
fn sweep_y(
    world: &World,
    pos: (f32, f32),
    size: (f32, f32),
    dy: f32,
    ignore_platforms: bool,
) -> (f32, bool, bool) {
    let (x, y) = pos;
    let (w, h) = size;
    let c0 = cell(x + COLLISION_EPS);
    let c1 = cell(x + w - COLLISION_EPS);
    if dy > 0.0 {
        let old_edge = y + h;
        let desired = old_edge + dy;
        for r in cell(old_edge + COLLISION_EPS)..=cell(desired) {
            for c in c0..=c1 {
                let platform_lands = !ignore_platforms
                    && world.is_platform(c, r)
                    && old_edge <= r as f32 + COLLISION_EPS;
                if world.is_solid(c, r) || platform_lands {
                    return (r as f32 - COLLISION_EPS - h, true, false);
                }
            }
        }
        (desired - h, false, false)
    } else if dy < 0.0 {
        let old_edge = y;
        let desired = old_edge + dy;
        let r_start = cell(old_edge - COLLISION_EPS);
        let r_end = cell(desired);
        for r in (r_end..=r_start).rev() {
            if (c0..=c1).any(|c| world.is_solid(c, r)) {
                return ((r + 1) as f32 + COLLISION_EPS, false, true);
            }
        }
        (desired, false, false)
    } else {
        (y, false, false)
    }
}

/// Standing exclusively on platforms (no solid ground under any part of the
/// feet) — the precondition for Down+Jump drop-through.
fn standing_on_platform_only(world: &World, pos: (f32, f32), size: (f32, f32)) -> bool {
    let (c0, c1, _, _) = cells(pos, size);
    let r = cell(pos.1 + size.1 + COLLISION_EPS);
    let mut on_platform = false;
    for c in c0..=c1 {
        if world.is_solid(c, r) {
            return false;
        }
        on_platform |= world.is_platform(c, r);
    }
    on_platform
}

/// Attempts the 1-tile auto-step over the obstacle ahead. On success returns
/// the fully resolved new position (lifted and advanced).
fn try_step_up(world: &World, pos: (f32, f32), size: (f32, f32), dx: f32) -> Option<(f32, f32)> {
    let (x, y) = pos;
    let (w, h) = size;
    let target_col = if dx > 0.0 {
        cell(x + w + COLLISION_EPS)
    } else {
        cell(x - COLLISION_EPS)
    };
    // Topmost solid cell of the obstacle within the body's rows.
    let r0 = cell(y + COLLISION_EPS);
    let r1 = cell(y + h - COLLISION_EPS);
    let top = (r0..=r1).find(|&r| world.is_solid(target_col, r))?;
    let lifted_y = top as f32 - h - COLLISION_EPS;
    let lift = y - lifted_y;
    if lift <= 0.0 || lift > AUTO_STEP_TILES + 2.0 * COLLISION_EPS {
        return None;
    }
    if aabb_overlaps_solid(world, (x, lifted_y), size) {
        return None;
    }
    let (nx, _) = sweep_x(world, (x, lifted_y), size, dx);
    if (nx - x).abs() <= COLLISION_EPS {
        return None; // stepping up wouldn't let us advance
    }
    Some((nx, lifted_y))
}

/// [`step_player_with_mods`] with no equipment modifiers — the original
/// signature, kept so existing callers (and call sites that predate the
/// loadout system) stay source-compatible.
pub fn step_player(
    world: &World,
    p: &mut PlayerPhysics,
    input: PlayerInput,
    dt: f32,
) -> StepResult {
    step_player_with_mods(world, p, input, dt, PhysicsMods::NONE)
}

/// Advances one player by `dt` seconds against the tile grid.
///
/// Handles: run accel/friction, hold-jump variable height, gravity/terminal
/// velocity, axis-separated AABB collision, 1-tile auto-step, platform
/// semi-solidity with Down+Jump drop, liquid physics (§3), cobweb slowdown,
/// fall-distance tracking, and equipment modifiers ([`PhysicsMods`]: Swift
/// Boots run speed, Gust Jar mid-air jump).
pub fn step_player_with_mods(
    world: &World,
    p: &mut PlayerPhysics,
    input: PlayerInput,
    dt: f32,
    mods: PhysicsMods,
) -> StepResult {
    let size = (PLAYER_WIDTH, PLAYER_HEIGHT);
    let mut out = StepResult::default();

    // Environment.
    let swimming = liquid_at_center(world, p.pos, size).is_some();
    out.swimming = swimming;
    out.in_water = aabb_touches_liquid(world, p.pos, size, LiquidKind::Water);
    out.in_lava = aabb_touches_liquid(world, p.pos, size, LiquidKind::Lava);
    out.in_cobweb = aabb_overlaps_tile(world, p.pos, size, TileId::Cobweb);

    // Jump intents (press edge).
    let jump_pressed = input.jump && !p.jump_was_held;
    p.jump_was_held = input.jump;

    if jump_pressed && input.down && p.on_ground && standing_on_platform_only(world, p.pos, size) {
        // Down+Jump: drop through the platform instead of jumping.
        p.drop_through = DROP_THROUGH_SECS;
        p.on_ground = false;
        p.jump_hold_left = 0.0;
    } else if swimming {
        p.jump_hold_left = 0.0;
        if jump_pressed {
            p.vel.1 = -SWIM_IMPULSE;
        }
    } else if jump_pressed && p.on_ground {
        p.vel.1 = -JUMP_SPEED;
        p.jump_hold_left = JUMP_HOLD_SECS;
        p.on_ground = false;
    } else if jump_pressed && !p.on_ground && p.air_jumps_used < mods.extra_air_jumps {
        // Gust Jar mid-air jump (§4.3, "second jump = 75% height"): same
        // impulse, shorter hold window — see [`air_jump_hold_secs`]. A
        // mid-air jump also negates the fall so far (§8 fall-damage
        // mitigations).
        p.air_jumps_used += 1;
        p.vel.1 = -JUMP_SPEED;
        p.jump_hold_left = air_jump_hold_secs();
        p.fall_distance = 0.0;
    }
    if !input.jump {
        p.jump_hold_left = 0.0;
    }

    // Vertical acceleration: held rise overrides gravity (§8).
    if p.jump_hold_left > 0.0 {
        p.vel.1 = -JUMP_SPEED;
        p.jump_hold_left = (p.jump_hold_left - dt).max(0.0);
    } else {
        let g = GRAVITY * if swimming { LIQUID_GRAVITY_MULT } else { 1.0 };
        p.vel.1 += g * dt;
    }
    let terminal = TERMINAL_VELOCITY * if swimming { LIQUID_TERMINAL_MULT } else { 1.0 };
    if p.vel.1 > terminal {
        p.vel.1 = terminal;
    }

    // Horizontal acceleration / friction.
    let dir = (input.right as i8 - input.left as i8) as f32;
    let max_run = RUN_MAX_SPEED
        * p.run_speed_mult
        * mods.speed_mult
        * if swimming { LIQUID_SPEED_MULT } else { 1.0 };
    if dir != 0.0 {
        p.vel.0 += RUN_ACCEL * dir * dt;
        p.vel.0 = p.vel.0.clamp(-max_run, max_run);
    } else if p.on_ground {
        let f = GROUND_FRICTION * dt;
        if p.vel.0.abs() <= f {
            p.vel.0 = 0.0;
        } else {
            p.vel.0 -= f * p.vel.0.signum();
        }
    }

    // Cobwebs clamp the resulting velocity (§2).
    if out.in_cobweb {
        p.vel.0 = p.vel.0.clamp(-COBWEB_MAX_SPEED, COBWEB_MAX_SPEED);
        p.vel.1 = p.vel.1.clamp(-COBWEB_MAX_SPEED, COBWEB_MAX_SPEED);
    }

    if p.drop_through > 0.0 {
        p.drop_through = (p.drop_through - dt).max(0.0);
    }

    // Horizontal move with auto-step.
    let dx = p.vel.0 * dt;
    if dx != 0.0 {
        let (nx, blocked) = sweep_x(world, p.pos, size, dx);
        if blocked && p.on_ground {
            if let Some(stepped) = try_step_up(world, p.pos, size, dx) {
                p.pos = stepped;
            } else {
                p.pos.0 = nx;
                p.vel.0 = 0.0;
            }
        } else {
            p.pos.0 = nx;
            if blocked {
                p.vel.0 = 0.0;
            }
        }
    }

    // Vertical move.
    let dy = p.vel.1 * dt;
    let was_falling = p.vel.1 > 0.0;
    // `landed` means a genuine airborne→grounded transition: while standing,
    // gravity makes vel.1 > 0 every tick, so `was_falling` alone would
    // re-report landing 60×/s (retriggering sounds/particles/Gust Jar).
    let was_airborne = !p.on_ground;
    let (ny, hit_floor, hit_ceiling) = sweep_y(world, p.pos, size, dy, p.drop_through > 0.0);
    let fell = (ny - p.pos.1).max(0.0);
    p.pos.1 = ny;

    if swimming {
        // Deep liquid breaks falls (§8); shallow puddles don't reach the
        // body center and so still hurt.
        p.fall_distance = 0.0;
        p.air_jumps_used = 0;
    } else if was_falling {
        p.fall_distance += fell;
    }

    p.on_ground = hit_floor;
    if hit_floor {
        if was_falling && was_airborne {
            out.landed = true;
            out.fall_distance = p.fall_distance;
        }
        p.fall_distance = 0.0;
        p.air_jumps_used = 0;
        p.vel.1 = 0.0;
    }
    if hit_ceiling {
        p.vel.1 = 0.0;
        p.jump_hold_left = 0.0;
        out.hit_ceiling = true;
    }
    out
}

/// Advances one item-drop entity by `dt` seconds: gravity capped at terminal
/// velocity (×§3 multipliers while submerged), axis-separated AABB collision
/// against solids (landing also on platform tops), and ground friction.
/// Returns whether the drop ended the step on the ground. Runs server-side
/// only, but lives here with the rest of the deterministic stepping.
pub fn step_item_drop(world: &World, pos: &mut (f32, f32), vel: &mut (f32, f32), dt: f32) -> bool {
    let size = hitbox::ITEM_DROP;
    let submerged = liquid_at_center(world, *pos, size).is_some();
    let (g_mult, t_mult) = if submerged {
        (LIQUID_GRAVITY_MULT, LIQUID_TERMINAL_MULT)
    } else {
        (1.0, 1.0)
    };
    vel.1 += GRAVITY * g_mult * dt;
    let terminal = TERMINAL_VELOCITY * t_mult;
    if vel.1 > terminal {
        vel.1 = terminal;
    }

    let dx = vel.0 * dt;
    if dx != 0.0 {
        let (nx, blocked) = sweep_x(world, *pos, size, dx);
        pos.0 = nx;
        if blocked {
            vel.0 = 0.0;
        }
    }

    let dy = vel.1 * dt;
    let (ny, hit_floor, hit_ceiling) = sweep_y(world, *pos, size, dy, false);
    pos.1 = ny;
    if hit_ceiling {
        vel.1 = 0.0;
    }
    if hit_floor {
        vel.1 = 0.0;
        let f = ITEM_FRICTION * dt;
        if vel.0.abs() <= f {
            vel.0 = 0.0;
        } else {
            vel.0 -= f * vel.0.signum();
        }
    }
    hit_floor
}

/// What one enemy/projectile body step observed (the enemy AI's sensory
/// inputs: ledges, walls, liquids).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BodyStep {
    pub on_ground: bool,
    /// The horizontal sweep hit a wall this step (after any auto-step).
    pub blocked_x: bool,
    pub hit_ceiling: bool,
    /// Body center is submerged in this liquid, if any.
    pub submerged: Option<LiquidKind>,
}

/// Advances a grounded enemy body (slimes, fighters) by `dt`: gravity under
/// the §0 terminal cap (×§3 liquid multipliers while submerged),
/// axis-separated AABB collision against solids + platform tops, cobweb
/// clamping (§2), and — when `auto_step` — the same 1-tile ledge step
/// players get (§5.2 Fighter AI). Knockback writes `vel` directly; this
/// only integrates.
pub fn step_enemy_body(
    world: &World,
    pos: &mut (f32, f32),
    vel: &mut (f32, f32),
    size: (f32, f32),
    dt: f32,
    auto_step: bool,
) -> BodyStep {
    let mut out = BodyStep {
        submerged: liquid_at_center(world, *pos, size),
        ..BodyStep::default()
    };
    let in_liquid = out.submerged.is_some();
    let g_mult = if in_liquid { LIQUID_GRAVITY_MULT } else { 1.0 };
    let t_mult = if in_liquid { LIQUID_TERMINAL_MULT } else { 1.0 };
    vel.1 += GRAVITY * g_mult * dt;
    let terminal = TERMINAL_VELOCITY * t_mult;
    if vel.1 > terminal {
        vel.1 = terminal;
    }
    if aabb_overlaps_tile(world, *pos, size, TileId::Cobweb) {
        vel.0 = vel.0.clamp(-COBWEB_MAX_SPEED, COBWEB_MAX_SPEED);
        vel.1 = vel.1.clamp(-COBWEB_MAX_SPEED, COBWEB_MAX_SPEED);
    }

    let dx = vel.0 * dt;
    if dx != 0.0 {
        let (nx, blocked) = sweep_x(world, *pos, size, dx);
        let mut resolved = false;
        if blocked && auto_step && vel.1 >= 0.0 {
            if let Some(stepped) = try_step_up(world, *pos, size, dx) {
                *pos = stepped;
                resolved = true;
            }
        }
        if !resolved {
            pos.0 = nx;
            out.blocked_x = blocked;
        }
    }

    let dy = vel.1 * dt;
    let (ny, hit_floor, hit_ceiling) = sweep_y(world, *pos, size, dy, false);
    pos.1 = ny;
    if hit_floor {
        vel.1 = 0.0;
        out.on_ground = true;
    }
    if hit_ceiling {
        vel.1 = 0.0;
        out.hit_ceiling = true;
    }
    out
}

/// Advances a flying body (fliers, projectiles) by `dt`: straight
/// axis-separated AABB collision against solids, no gravity (callers add
/// their own, e.g. arrows at ×0.35). Blocked axes zero their velocity
/// component unless the caller restores it (the Demon Eye reflects instead).
pub fn step_flier_body(
    world: &World,
    pos: &mut (f32, f32),
    vel: &mut (f32, f32),
    size: (f32, f32),
    dt: f32,
) -> BodyStep {
    let mut out = BodyStep {
        submerged: liquid_at_center(world, *pos, size),
        ..BodyStep::default()
    };
    let dx = vel.0 * dt;
    if dx != 0.0 {
        let (nx, blocked) = sweep_x(world, *pos, size, dx);
        pos.0 = nx;
        out.blocked_x = blocked;
    }
    let dy = vel.1 * dt;
    if dy != 0.0 {
        // Fliers pass over platforms (they only collide with solids).
        let (ny, hit_floor, hit_ceiling) = sweep_y(world, *pos, size, dy, true);
        pos.1 = ny;
        out.on_ground = hit_floor;
        out.hit_ceiling = hit_ceiling;
    }
    out
}

/// Whether a straight line between two points crosses any solid tile —
/// the §5.2 line-of-sight test (Ash Demon volleys). Samples the segment at
/// quarter-tile steps; good enough for AI.
pub fn line_of_sight(world: &World, from: (f32, f32), to: (f32, f32)) -> bool {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let dist = (dx * dx + dy * dy).sqrt();
    let steps = (dist * 4.0).ceil().max(1.0) as u32;
    for i in 0..=steps {
        let f = i as f32 / steps as f32;
        let (x, y) = (from.0 + dx * f, from.1 + dy * f);
        if world.is_solid(x.floor() as i32, y.floor() as i32) {
            return false;
        }
    }
    true
}

/// Advances one falling-tile entity (§2 tile 4: unsupported sand) by `dt`
/// seconds: a straight vertical fall under gravity capped at the §0 terminal
/// velocity (×§3 multipliers while submerged), colliding with **solid** tiles
/// only — a falling tile passes through platforms and non-solid fixtures,
/// since §2 keys its support on "no solid tile below". Returns `true` when
/// the step ended resting on a solid (the server then converts the entity
/// back into a tile). Server-side only, but deterministic stepping lives
/// here with the rest.
pub fn step_falling_tile(
    world: &World,
    pos: &mut (f32, f32),
    vel: &mut (f32, f32),
    dt: f32,
) -> bool {
    let size = hitbox::FALLING_TILE;
    let submerged = liquid_at_center(world, *pos, size).is_some();
    let (g_mult, t_mult) = if submerged {
        (LIQUID_GRAVITY_MULT, LIQUID_TERMINAL_MULT)
    } else {
        (1.0, 1.0)
    };
    vel.1 += GRAVITY * g_mult * dt;
    let terminal = TERMINAL_VELOCITY * t_mult;
    if vel.1 > terminal {
        vel.1 = terminal;
    }
    let (ny, hit_floor, _) = sweep_y(world, *pos, size, vel.1 * dt, true);
    pos.1 = ny;
    if hit_floor {
        vel.1 = 0.0;
    }
    hit_floor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{Liquid, Tile};
    use crate::DT;

    /// Builds a world from ASCII art. `#` stone, `-` platform, `c` cobweb,
    /// `w` full water, `.`/space air.
    fn world_from_ascii(rows: &[&str]) -> World {
        let mut w = World::new(rows[0].len() as u32, rows.len() as u32);
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.chars().enumerate() {
                let t = match ch {
                    '#' => Tile::of(TileId::Stone),
                    '-' => Tile::of(TileId::Platform),
                    'c' => Tile::of(TileId::Cobweb),
                    'w' => Tile {
                        liquid: Liquid::new(LiquidKind::Water, 8),
                        ..Tile::AIR
                    },
                    _ => Tile::AIR,
                };
                w.set_tile(x as u32, y as u32, t);
            }
        }
        w
    }

    /// A flat 60-wide world with a solid floor whose top is at y = 10.
    fn flat_world() -> World {
        let mut rows = vec![".".repeat(60); 10];
        rows.push("#".repeat(60));
        rows.push("#".repeat(60));
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        world_from_ascii(&refs)
    }

    fn settle(world: &World, p: &mut PlayerPhysics) {
        for _ in 0..10 {
            step_player(world, p, PlayerInput::default(), DT);
        }
    }

    const RIGHT: PlayerInput = PlayerInput {
        left: false,
        right: true,
        jump: false,
        down: false,
    };
    const JUMP: PlayerInput = PlayerInput {
        left: false,
        right: false,
        jump: true,
        down: false,
    };

    #[test]
    fn walks_on_flat_ground() {
        let world = flat_world();
        let mut p = PlayerPhysics::from_feet(5.0, 10.0);
        settle(&world, &mut p);
        assert!(p.on_ground);
        let start_x = p.center().0;
        for _ in 0..120 {
            let r = step_player(&world, &mut p, RIGHT, DT);
            assert!(p.on_ground, "stayed grounded while walking");
            assert!(!r.landed, "landed must not re-fire while grounded");
        }
        assert_eq!(p.vel.0, RUN_MAX_SPEED, "reaches max run speed exactly");
        let moved = p.center().0 - start_x;
        // 2 s of walking: ~0.6 s accelerating, then full speed.
        assert!((15.0..22.6).contains(&moved), "moved {moved}");
        assert!((p.feet_y() - 10.0).abs() < 0.01, "feet stay on the floor");

        // Releasing input: friction stops the player.
        for _ in 0..30 {
            step_player(&world, &mut p, PlayerInput::default(), DT);
        }
        assert_eq!(p.vel.0, 0.0);
    }

    #[test]
    fn jump_apex_is_about_six_and_a_half_tiles() {
        let world = flat_world();
        let mut p = PlayerPhysics::from_feet(30.0, 10.0);
        settle(&world, &mut p);
        let start = p.feet_y();
        let mut apex = start;
        for _ in 0..120 {
            step_player(&world, &mut p, JUMP, DT);
            apex = apex.min(p.feet_y());
        }
        let rise = start - apex;
        assert!((6.0..7.0).contains(&rise), "full-hold jump rose {rise}");
        assert!(p.on_ground, "came back down");

        // Tap jump (one tick) is much shorter.
        let mut p = PlayerPhysics::from_feet(30.0, 10.0);
        settle(&world, &mut p);
        let mut tap_apex = p.feet_y();
        step_player(&world, &mut p, JUMP, DT);
        for _ in 0..120 {
            step_player(&world, &mut p, PlayerInput::default(), DT);
            tap_apex = tap_apex.min(p.feet_y());
        }
        let tap_rise = start - tap_apex;
        assert!(
            tap_rise < rise / 2.0,
            "tap jump ({tap_rise}) much lower than full hold ({rise})"
        );
    }

    #[test]
    fn landed_fires_exactly_once_per_fall() {
        let world = flat_world();
        let mut p = PlayerPhysics::new((5.0, 2.0)); // starts airborne
        let mut landings = 0;
        for _ in 0..240 {
            let r = step_player(&world, &mut p, PlayerInput::default(), DT);
            if r.landed {
                landings += 1;
                assert!(r.fall_distance > 0.0, "landing reports the real fall");
            }
        }
        assert!(p.on_ground);
        assert_eq!(landings, 1, "one fall, one landing event");
    }

    #[test]
    fn terminal_velocity_caps_fall_speed() {
        let world = World::new(10, 200); // all air; border is solid
        let mut p = PlayerPhysics::new((4.0, 0.0));
        let mut max_v: f32 = 0.0;
        for _ in 0..200 {
            step_player(&world, &mut p, PlayerInput::default(), DT);
            max_v = max_v.max(p.vel.1);
        }
        assert_eq!(p.vel.1, TERMINAL_VELOCITY);
        assert!(max_v <= TERMINAL_VELOCITY);
        assert!(p.fall_distance > 100.0, "fall distance accumulates");
    }

    #[test]
    fn cannot_pass_through_walls() {
        // Wall occupying column 12, floor top at y = 10.
        let mut rows = vec!["............#.".to_string(); 10];
        rows.push("##############".to_string());
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let world = world_from_ascii(&refs);
        let mut p = PlayerPhysics::from_feet(5.0, 10.0);
        settle(&world, &mut p);
        for _ in 0..240 {
            step_player(&world, &mut p, RIGHT, DT);
        }
        let right_edge = p.pos.0 + PLAYER_WIDTH;
        assert!(right_edge <= 12.0, "stopped at the wall ({right_edge})");
        assert!(right_edge > 11.9, "made it all the way to the wall");
        assert_eq!(p.vel.0, 0.0);
    }

    #[test]
    fn auto_steps_one_tile_ledges() {
        // Floor top at y=10 for x < 20, elevated floor top at y=9 after.
        let mut rows = vec!["".to_string(); 12];
        for (y, row) in rows.iter_mut().enumerate() {
            *row = (0..60)
                .map(|x| {
                    let floor_top = if x < 20 { 10 } else { 9 };
                    if y >= floor_top {
                        '#'
                    } else {
                        '.'
                    }
                })
                .collect();
        }
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let world = world_from_ascii(&refs);
        let mut p = PlayerPhysics::from_feet(15.0, 10.0);
        settle(&world, &mut p);
        for _ in 0..120 {
            step_player(&world, &mut p, RIGHT, DT);
            assert!(p.on_ground, "auto-step keeps the player grounded");
        }
        assert!(p.center().0 > 25.0, "walked past the ledge");
        assert!(
            (p.feet_y() - 9.0).abs() < 0.01,
            "standing on the high floor"
        );
    }

    #[test]
    fn cannot_auto_step_two_tile_walls() {
        // 2-high wall standing on the floor at x = 8 (floor top y = 8).
        let mut rows = vec!["............".to_string(); 8];
        rows[6] = "........#...".to_string();
        rows[7] = "........#...".to_string();
        rows.push("############".to_string());
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let world = world_from_ascii(&refs);
        let mut p = PlayerPhysics::from_feet(3.0, 8.0);
        settle(&world, &mut p);
        for _ in 0..240 {
            step_player(&world, &mut p, RIGHT, DT);
        }
        assert!(p.pos.0 + PLAYER_WIDTH <= 8.0, "blocked by the 2-tile wall");
        assert!((p.feet_y() - 8.0).abs() < 0.01, "did not climb it");
    }

    #[test]
    fn platforms_are_semi_solid() {
        let world = world_from_ascii(&[
            "..........",
            "..........",
            "..........",
            "----------",
            "..........",
            "..........",
            "##########",
        ]);
        // Lands on the platform from above.
        let mut p = PlayerPhysics::new((4.0, 0.0));
        for _ in 0..120 {
            step_player(&world, &mut p, PlayerInput::default(), DT);
        }
        assert!((p.feet_y() - 3.0).abs() < 0.01, "landed on platform");
        assert!(p.on_ground);

        // Down+Jump drops through it and lands on the ground below.
        let drop = PlayerInput {
            down: true,
            jump: true,
            ..PlayerInput::default()
        };
        step_player(&world, &mut p, drop, DT);
        for _ in 0..120 {
            step_player(&world, &mut p, PlayerInput::default(), DT);
        }
        assert!((p.feet_y() - 6.0).abs() < 0.01, "fell to the floor");

        // Jumping from the floor passes up through the platform...
        let mut landed_back = false;
        for tick in 0..200 {
            let input = if tick < 20 {
                JUMP
            } else {
                PlayerInput::default()
            };
            let r = step_player(&world, &mut p, input, DT);
            landed_back |= r.landed && (p.feet_y() - 3.0).abs() < 0.01;
        }
        // ...and comes back to rest on top of it.
        assert!(landed_back, "rose through the platform and landed on it");
        assert!((p.feet_y() - 3.0).abs() < 0.01);
    }

    #[test]
    fn cobwebs_clamp_velocity() {
        let mut rows = vec!["....".to_string(); 30];
        for row in rows.iter_mut().take(14).skip(10) {
            *row = "cccc".to_string();
        }
        rows.push("####".to_string());
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let world = world_from_ascii(&refs);
        let mut p = PlayerPhysics::new((1.0, 0.0));
        let mut saw_web = false;
        for _ in 0..400 {
            let r = step_player(&world, &mut p, PlayerInput::default(), DT);
            if r.in_cobweb {
                saw_web = true;
                assert!(p.vel.1 <= COBWEB_MAX_SPEED + 1e-3, "vel {} in web", p.vel.1);
            }
        }
        assert!(saw_web);
        assert!(p.on_ground, "eventually fell through to the floor");
    }

    #[test]
    fn water_softens_physics_and_falls() {
        let world = world_from_ascii(&[
            "..........",
            "..........",
            "wwwwwwwwww",
            "wwwwwwwwww",
            "wwwwwwwwww",
            "wwwwwwwwww",
            "##########",
        ]);
        let mut p = PlayerPhysics::new((4.0, 0.0));
        let mut landed_fall = None;
        let mut max_submerged_v: f32 = 0.0;
        for _ in 0..300 {
            let r = step_player(&world, &mut p, PlayerInput::default(), DT);
            if r.in_water {
                max_submerged_v = max_submerged_v.max(p.vel.1);
            }
            if r.landed {
                landed_fall = Some(r.fall_distance);
            }
        }
        assert!(p.on_ground);
        assert_eq!(landed_fall, Some(0.0), "deep water negates the fall");
        assert!(
            max_submerged_v <= TERMINAL_VELOCITY * LIQUID_TERMINAL_MULT + 1e-3,
            "water terminal velocity respected ({max_submerged_v})"
        );

        // Swim impulse: a jump press while submerged kicks upward at 12 t/s.
        let r = step_player(&world, &mut p, JUMP, DT);
        assert!(r.in_water);
        assert!(r.swimming, "body center is submerged");
        assert!(p.vel.1 < -SWIM_IMPULSE * 0.8, "swim impulse applied");
    }

    #[test]
    fn ankle_deep_water_wades_without_swimming() {
        // One row of water resting on the floor: the AABB touches it, but
        // the body center (height 2.75 → center 1.375 above the feet) stays
        // dry, so this is wading, not swimming.
        let world = world_from_ascii(&[
            "..........",
            "..........",
            "..........",
            "wwwwwwwwww",
            "##########",
        ]);
        let mut p = PlayerPhysics::from_feet(5.0, 4.0);
        settle(&world, &mut p);
        let r = step_player(&world, &mut p, PlayerInput::default(), DT);
        assert!(r.in_water, "feet are in the water");
        assert!(!r.swimming, "ankle-deep is not submerged");
    }

    #[test]
    fn fall_damage_formula() {
        assert_eq!(fall_damage(0.0), 0);
        assert_eq!(fall_damage(25.0), 0);
        assert_eq!(fall_damage(26.0), 10);
        assert_eq!(fall_damage(30.0), 50);
    }

    #[test]
    fn item_drops_fall_land_and_stop() {
        let world = flat_world(); // floor top at y = 10
        let mut pos = (5.0, 2.0);
        let mut vel = (3.0, -2.0); // a spawn impulse
        let mut grounded = false;
        for _ in 0..240 {
            grounded = step_item_drop(&world, &mut pos, &mut vel, DT);
        }
        assert!(grounded, "came to rest on the floor");
        let (_, h) = hitbox::ITEM_DROP;
        assert!(
            (pos.1 + h - 10.0).abs() < 0.01,
            "rests on the floor top, bottom at {}",
            pos.1 + h
        );
        assert_eq!(vel, (0.0, 0.0), "friction stopped the slide");
        assert!(pos.0 > 5.0, "slid in the impulse direction first");
    }

    #[test]
    fn item_drops_respect_walls_and_terminal_velocity() {
        // Wall at column 12 (same map as the player wall test).
        let mut rows = vec!["............#.".to_string(); 30];
        rows.push("##############".to_string());
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let world = world_from_ascii(&refs);
        let mut pos = (5.0, 0.0);
        let mut vel = (50.0, 0.0);
        let mut max_vy: f32 = 0.0;
        for _ in 0..240 {
            step_item_drop(&world, &mut pos, &mut vel, DT);
            max_vy = max_vy.max(vel.1);
        }
        assert!(max_vy <= TERMINAL_VELOCITY);
        let (w, _) = hitbox::ITEM_DROP;
        assert!(pos.0 + w <= 12.0, "stopped at the wall ({})", pos.0 + w);
    }

    #[test]
    fn item_drops_rest_on_platforms() {
        let world = world_from_ascii(&[
            "..........",
            "..........",
            "----------",
            "..........",
            "##########",
        ]);
        let mut pos = (4.0, 0.0);
        let mut vel = (0.0, 0.0);
        for _ in 0..120 {
            step_item_drop(&world, &mut pos, &mut vel, DT);
        }
        let (_, h) = hitbox::ITEM_DROP;
        assert!(
            (pos.1 + h - 2.0).abs() < 0.01,
            "landed on the platform, bottom at {}",
            pos.1 + h
        );
    }

    #[test]
    fn swift_boots_mod_raises_max_run_speed() {
        let world = flat_world();
        let mods = PhysicsMods {
            speed_mult: crate::items::SWIFT_BOOTS_SPEED_MULT,
            ..PhysicsMods::NONE
        };
        let mut p = PlayerPhysics::from_feet(5.0, 10.0);
        settle(&world, &mut p);
        for _ in 0..120 {
            step_player_with_mods(&world, &mut p, RIGHT, DT, mods);
        }
        assert_eq!(
            p.vel.0,
            RUN_MAX_SPEED * crate::items::SWIFT_BOOTS_SPEED_MULT
        );
        // And without mods the plain cap still holds.
        let mut q = PlayerPhysics::from_feet(5.0, 10.0);
        settle(&world, &mut q);
        for _ in 0..120 {
            step_player(&world, &mut q, RIGHT, DT);
        }
        assert_eq!(q.vel.0, RUN_MAX_SPEED);
    }

    /// A flat world with lots of headroom (floor top y = 30) so stacked
    /// jumps never reach the solid world border above row 0.
    fn tall_flat_world() -> World {
        let mut rows = vec![".".repeat(60); 30];
        rows.push("#".repeat(60));
        rows.push("#".repeat(60));
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        world_from_ascii(&refs)
    }

    #[test]
    fn gust_jar_mod_allows_one_air_jump_at_75_percent_height() {
        let world = tall_flat_world();
        let mods = PhysicsMods {
            extra_air_jumps: 1,
            ..PhysicsMods::NONE
        };
        // Reference: full-hold ground jump apex.
        let mut p = PlayerPhysics::from_feet(30.0, 30.0);
        settle(&world, &mut p);
        let start = p.feet_y();
        let mut full_apex = start;
        for _ in 0..120 {
            step_player(&world, &mut p, JUMP, DT);
            full_apex = full_apex.min(p.feet_y());
        }
        let full_rise = start - full_apex;

        // Air jump from the ground-jump apex: release at apex, re-press.
        let mut p = PlayerPhysics::from_feet(30.0, 30.0);
        settle(&world, &mut p);
        while p.vel.1 < 0.0 || p.on_ground {
            step_player_with_mods(&world, &mut p, JUMP, DT, mods);
        }
        let apex = p.feet_y();
        step_player_with_mods(&world, &mut p, PlayerInput::default(), DT, mods); // release
        let mut air_apex = p.feet_y();
        step_player_with_mods(&world, &mut p, JUMP, DT, mods); // re-press
        assert_eq!(p.air_jumps_used, 1, "air jump consumed");
        for _ in 0..150 {
            step_player_with_mods(&world, &mut p, JUMP, DT, mods);
            air_apex = air_apex.min(p.feet_y());
        }
        let air_rise = apex - air_apex;
        let ratio = air_rise / full_rise;
        assert!(
            (0.70..0.80).contains(&ratio),
            "air jump rose {air_rise} vs full {full_rise} (ratio {ratio})"
        );

        // Without the mod, the same re-press does nothing mid-air.
        let mut q = PlayerPhysics::from_feet(30.0, 30.0);
        settle(&world, &mut q);
        while q.vel.1 < 0.0 || q.on_ground {
            step_player(&world, &mut q, JUMP, DT);
        }
        step_player(&world, &mut q, PlayerInput::default(), DT);
        let vy_before = q.vel.1;
        step_player(&world, &mut q, JUMP, DT);
        assert!(q.vel.1 >= vy_before, "no air jump without the mod");

        // Landing resets the allowance.
        for _ in 0..200 {
            step_player_with_mods(&world, &mut p, PlayerInput::default(), DT, mods);
        }
        assert!(p.on_ground);
        assert_eq!(p.air_jumps_used, 0);
    }

    #[test]
    fn air_jump_negates_accumulated_fall_distance() {
        let world = World::new(10, 300); // tall free fall
        let mods = PhysicsMods {
            extra_air_jumps: 1,
            ..PhysicsMods::NONE
        };
        let mut p = PlayerPhysics::new((4.0, 0.0));
        for _ in 0..120 {
            step_player_with_mods(&world, &mut p, PlayerInput::default(), DT, mods);
        }
        assert!(p.fall_distance > SAFE_FALL_TILES, "fell far");
        step_player_with_mods(&world, &mut p, JUMP, DT, mods);
        assert_eq!(p.fall_distance, 0.0, "mid-air jump resets the fall");
    }

    #[test]
    fn enemy_body_walks_steps_and_blocks() {
        // Floor top y=10 for x<20, y=9 after (1-tile ledge), wall at x=30.
        let mut rows: Vec<String> = (0..12)
            .map(|y| {
                (0..40)
                    .map(|x| {
                        let floor_top = if x < 20 { 10 } else { 9 };
                        if y >= floor_top || (x == 30 && y >= 6) {
                            '#'
                        } else {
                            '.'
                        }
                    })
                    .collect()
            })
            .collect();
        rows.push("#".repeat(40));
        let refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let world = world_from_ascii(&refs);
        let size = (1.25, 2.75);
        let mut pos = (15.0, 10.0 - size.1 - COLLISION_EPS);
        let mut vel = (0.0f32, 0.0f32);
        let mut blocked = false;
        for _ in 0..600 {
            vel.0 = 3.2;
            // (Like the player, the step engages one tick after first
            // contact — the body must be flush against the ledge.)
            blocked |= step_enemy_body(&world, &mut pos, &mut vel, size, DT, true).blocked_x;
        }
        assert!(blocked, "reached and reported the wall");
        assert!(
            (pos.0 + size.0 - 30.0).abs() < 0.05,
            "walked to the wall (edge at {})",
            pos.0 + size.0
        );
        assert!((pos.1 + size.1 - 9.0).abs() < 0.05, "auto-stepped the ledge");

        // Without auto-step the same body is blocked at the ledge instead.
        let mut pos = (15.0, 10.0 - size.1 - COLLISION_EPS);
        let mut vel = (3.2f32, 0.0f32);
        let mut blocked_at = None;
        for _ in 0..600 {
            vel.0 = 3.2;
            let r = step_enemy_body(&world, &mut pos, &mut vel, size, DT, false);
            if r.blocked_x {
                blocked_at = Some(pos.0 + size.0);
                break;
            }
        }
        assert!(
            blocked_at.is_some_and(|x| (x - 20.0).abs() < 0.05),
            "no auto-step: blocked at the ledge ({blocked_at:?})"
        );
    }

    #[test]
    fn flier_body_reports_collisions_without_gravity() {
        let world = world_from_ascii(&[
            "##########",
            "..........",
            "..........",
            "..........",
            "##########",
        ]);
        let size = (1.0, 1.0);
        let mut pos = (4.0, 2.0);
        let mut vel = (0.0, -10.0);
        let mut hit_ceiling = false;
        for _ in 0..60 {
            let r = step_flier_body(&world, &mut pos, &mut vel, size, DT);
            hit_ceiling |= r.hit_ceiling;
        }
        assert!(hit_ceiling);
        assert!(pos.1 >= 1.0 - 0.01, "did not clip into the ceiling");
        // No gravity was applied: vy is whatever the caller left there.
        assert_eq!(vel.0, 0.0);

        let mut pos = (4.0, 2.0);
        let mut vel = (12.0, 0.0);
        let mut blocked = false;
        for _ in 0..120 {
            let r = step_flier_body(&world, &mut pos, &mut vel, size, DT);
            blocked |= r.blocked_x;
        }
        assert!(blocked, "hit the right border wall");
    }

    #[test]
    fn line_of_sight_blocked_by_solids() {
        let world = world_from_ascii(&[
            "..........",
            "....#.....",
            "....#.....",
            "....#.....",
            "##########",
        ]);
        assert!(!line_of_sight(&world, (1.5, 2.5), (8.5, 2.5)), "wall blocks");
        assert!(line_of_sight(&world, (1.5, 0.5), (8.5, 0.5)), "over the top");
        assert!(line_of_sight(&world, (1.5, 2.5), (3.5, 2.5)), "same side");
    }

    #[test]
    fn hits_ceilings() {
        let world = world_from_ascii(&[
            "##########",
            "..........",
            "..........",
            "..........",
            "##########",
        ]);
        let mut p = PlayerPhysics::from_feet(5.0, 4.0);
        settle(&world, &mut p);
        let mut hit = false;
        for _ in 0..30 {
            hit |= step_player(&world, &mut p, JUMP, DT).hit_ceiling;
        }
        assert!(hit, "bumped the ceiling");
        assert!(p.pos.1 >= 1.0 - 0.01, "did not clip into the ceiling");
    }
}
