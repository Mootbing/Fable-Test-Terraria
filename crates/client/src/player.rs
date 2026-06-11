//! Players: own-player prediction (fixed 60 Hz accumulator over the shared
//! physics step, decoupled from render FPS) and remote-player snapshot
//! interpolation (~100 ms behind, per ARCHITECTURE.md "Authority model").

use std::collections::VecDeque;

use ferraria_shared::items::ItemId;
use ferraria_shared::physics::{
    step_player_with_mods, PhysicsMods, PlayerInput, PlayerPhysics, StepResult,
};
use ferraria_shared::protocol::{anim, ActiveDebuff, ClientMessage, Debuff};
use ferraria_shared::{DT, PLAYER_BASE_MAX_HP, SNAPSHOT_INTERVAL_TICKS, TICK_RATE};

/// Cap on frame time fed into the fixed-step accumulator, so a hidden tab
/// resuming doesn't burst hundreds of physics ticks.
const MAX_FRAME_DT: f32 = 0.25;

/// Render remote players this far in the past, interpolating between
/// snapshots (ARCHITECTURE.md: ~100 ms).
pub const INTERP_DELAY: f64 = 0.10;

/// On snapshot gaps, extrapolate along the last velocity at most this long.
const MAX_EXTRAPOLATION: f64 = 0.10;

/// An own-id `PlayerMoved` whose position differs from our prediction by
/// more than this is an authoritative server correction — snap to it.
pub const CORRECTION_SNAP_TILES: f32 = 1.0;

/// Snapshots buffered per remote player (~3 s at 20/s).
const SNAPSHOT_BUFFER: usize = 64;

// ---- Own player -------------------------------------------------------------

/// The locally predicted own player.
pub struct OwnPlayer {
    pub phys: PlayerPhysics,
    pub facing: i8,
    accumulator: f32,
    tick: u64,
    last_step: StepResult,
    /// Last `PlayerState` actually sent; suppresses idle resends so the
    /// server's `moved` flag (and rebroadcast traffic) stays quiet.
    last_sent: Option<ClientMessage>,
    /// A Gust Jar mid-air jump happened since the last sent state. One-shot:
    /// ORed into the next `PlayerState` anim byte so the server resets its
    /// observed fall distance (§8 / `protocol::anim::AIR_JUMPED`), then
    /// cleared.
    air_jumped: bool,
}

impl OwnPlayer {
    /// Standing on the spawn platform — must match the server's spawn
    /// placement (`spawn` is the air tile whose row below is the platform).
    pub fn at_spawn(spawn: (u32, u32)) -> OwnPlayer {
        OwnPlayer::at(PlayerPhysics::from_feet(
            spawn.0 as f32 + 0.5,
            (spawn.1 + 1) as f32,
        ))
    }

    fn at(phys: PlayerPhysics) -> OwnPlayer {
        OwnPlayer {
            phys,
            facing: 1,
            accumulator: 0.0,
            tick: 0,
            last_step: StepResult::default(),
            last_sent: None,
            air_jumped: false,
        }
    }

    /// Advances the fixed-step simulation by one render frame, returning the
    /// `PlayerState` messages to send (one per 3rd sim tick, when changed).
    /// `frozen` skips stepping (chunk under us not loaded yet) without
    /// banking time in the accumulator. `mods` carries the equipment physics
    /// modifiers from the synced inventory (`loadout::physics_mods`) so
    /// prediction matches the server's expectations.
    pub fn update(
        &mut self,
        world: &ferraria_shared::world::World,
        input: PlayerInput,
        frame_dt: f32,
        frozen: bool,
        mods: PhysicsMods,
    ) -> Vec<ClientMessage> {
        let mut out = Vec::new();
        if frozen {
            self.accumulator = 0.0;
            return out;
        }
        self.accumulator += frame_dt.min(MAX_FRAME_DT);
        while self.accumulator >= DT {
            self.accumulator -= DT;
            if input.left != input.right {
                self.facing = if input.right { 1 } else { -1 };
            }
            // The shared step consumes Gust Jar air jumps itself; an
            // `air_jumps_used` increment across the step is the only signal
            // (the counter resets to 0 on landing/swimming, never +1s).
            let jumps_before = self.phys.air_jumps_used;
            self.last_step = step_player_with_mods(world, &mut self.phys, input, DT, mods);
            if self.phys.air_jumps_used > jumps_before {
                self.air_jumped = true;
            }
            self.tick += 1;
            if self.tick.is_multiple_of(SNAPSHOT_INTERVAL_TICKS as u64) {
                let mut anim = self.anim_flags();
                if self.air_jumped {
                    anim |= anim::AIR_JUMPED; // one-shot (§8 fall negation)
                }
                let state = ClientMessage::PlayerState {
                    pos: self.phys.pos,
                    vel: self.phys.vel,
                    facing: self.facing,
                    anim,
                };
                if self.last_sent.as_ref() != Some(&state) {
                    self.last_sent = Some(state.clone());
                    out.push(state);
                    self.air_jumped = false;
                }
            }
        }
        out
    }

    /// `ServerMessage::PlayerKnockback` (enemy contact / projectile hit):
    /// movement is client-authoritative, so the server can't shove us — it
    /// asks, and we add the impulse to the predicted velocity.
    pub fn apply_knockback(&mut self, vx: f32, vy: f32) {
        self.phys.vel.0 += vx;
        self.phys.vel.1 += vy;
        // A shove interrupts a held-jump rise, like releasing the key.
        self.phys.jump_hold_left = 0.0;
    }

    /// Applies an authoritative own-id correction (teleport rejection /
    /// reconnect reclaim): adopt the server's position outright and clear
    /// transient motion state — a held-jump rise or a live platform
    /// drop-through must not keep acting from the corrected position.
    pub fn apply_correction(&mut self, pos: (f32, f32), vel: (f32, f32)) {
        self.phys.pos = pos;
        self.phys.vel = vel;
        self.phys.fall_distance = 0.0;
        self.phys.jump_hold_left = 0.0;
        self.phys.drop_through = 0.0;
        self.phys.on_ground = false; // re-resolved by the next step
        self.last_sent = None; // force the next snapshot out
    }

    pub fn anim_flags(&self) -> u8 {
        let mut flags = 0;
        if self.phys.on_ground {
            flags |= anim::GROUNDED;
        }
        // Submerged, not merely touching a liquid cell (protocol.rs
        // documents the bit as "Submerged / swim animation"): wading
        // ankle-deep must not broadcast the swim animation.
        if self.last_step.swimming {
            flags |= anim::IN_LIQUID;
        }
        flags
    }
}

// ---- Remote players ----------------------------------------------------------

/// One timestamped `PlayerMoved` sample.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub t: f64,
    pub pos: (f32, f32),
    pub vel: (f32, f32),
    pub facing: i8,
    pub anim: u8,
}

/// Another player, rendered from buffered snapshots.
pub struct RemotePlayer {
    pub name: String,
    pub held_item: Option<ItemId>,
    /// Server-authoritative HP mirror (`PlayerHealth` broadcasts) — drives
    /// the small health bar over hurt teammates.
    pub hp: u16,
    pub max_hp: u16,
    /// Dead until `PlayerRespawned` (§8): hidden while down, like the
    /// server expects (a corpse can't be hit, lit, or interpolated).
    pub dead: bool,
    /// Wall-clock time their Darkness debuff wears off (§5.2/§10: remote
    /// victims' glow is dimmed too); `NEG_INFINITY` when not afflicted.
    darkness_until: f64,
    snaps: VecDeque<Snapshot>,
}

impl RemotePlayer {
    pub fn new(name: String, pos: (f32, f32), now: f64) -> RemotePlayer {
        let mut snaps = VecDeque::new();
        snaps.push_back(Snapshot {
            t: now,
            pos,
            vel: (0.0, 0.0),
            facing: 1,
            anim: anim::GROUNDED,
        });
        RemotePlayer {
            name,
            held_item: None,
            hp: PLAYER_BASE_MAX_HP as u16,
            max_hp: PLAYER_BASE_MAX_HP as u16,
            dead: false,
            darkness_until: f64::NEG_INFINITY,
            snaps,
        }
    }

    pub fn push(&mut self, snap: Snapshot) {
        if self.snaps.len() >= SNAPSHOT_BUFFER {
            self.snaps.pop_front();
        }
        self.snaps.push_back(snap);
    }

    /// `PlayerDebuffs` replacement list: remember when Darkness wears off
    /// (the only remote-relevant debuff — it dims their glow, §10).
    pub fn set_debuffs(&mut self, debuffs: &[ActiveDebuff], now: f64) {
        self.darkness_until = debuffs
            .iter()
            .find(|d| d.debuff == Debuff::Darkness)
            .map(|d| now + d.remaining_ticks as f64 / TICK_RATE as f64)
            .unwrap_or(f64::NEG_INFINITY);
    }

    pub fn has_darkness(&self, now: f64) -> bool {
        now < self.darkness_until
    }

    /// `PlayerRespawned`: drop the stale snapshot history so they pop in at
    /// the respawn point instead of interpolating across the map.
    pub fn reset_to(&mut self, pos: (f32, f32), now: f64) {
        self.snaps.clear();
        self.snaps.push_back(Snapshot {
            t: now,
            pos,
            vel: (0.0, 0.0),
            facing: 1,
            anim: anim::GROUNDED,
        });
    }

    /// State to draw at `render_t` (typically `now - INTERP_DELAY`):
    /// interpolated between the bracketing snapshots, or extrapolated up to
    /// [`MAX_EXTRAPOLATION`] past the newest one. Prunes consumed history.
    pub fn sample(&mut self, render_t: f64) -> Snapshot {
        while self.snaps.len() >= 2 && self.snaps[1].t <= render_t {
            self.snaps.pop_front();
        }
        // Invariant: the constructor seeds one snapshot and pruning keeps >= 1.
        let a = match self.snaps.front() {
            Some(&a) => a,
            None => {
                return Snapshot {
                    t: render_t,
                    pos: (0.0, 0.0),
                    vel: (0.0, 0.0),
                    facing: 1,
                    anim: 0,
                }
            }
        };
        match self.snaps.get(1) {
            Some(&b) => {
                let span = b.t - a.t;
                let f = if span > 0.0 {
                    (((render_t - a.t) / span).clamp(0.0, 1.0)) as f32
                } else {
                    1.0
                };
                Snapshot {
                    t: render_t,
                    pos: (
                        a.pos.0 + (b.pos.0 - a.pos.0) * f,
                        a.pos.1 + (b.pos.1 - a.pos.1) * f,
                    ),
                    vel: b.vel,
                    facing: b.facing,
                    anim: b.anim,
                }
            }
            None => {
                let dt = (render_t - a.t).clamp(0.0, MAX_EXTRAPOLATION) as f32;
                Snapshot {
                    t: render_t,
                    pos: (a.pos.0 + a.vel.0 * dt, a.pos.1 + a.vel.1 * dt),
                    ..a
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferraria_shared::tiles::{Tile, TileId};
    use ferraria_shared::world::World;

    fn flat_world() -> World {
        let mut w = World::new(64, 64);
        for x in 0..64 {
            for y in 40..64 {
                w.set_tile(x, y, Tile::of(TileId::Stone));
            }
        }
        w
    }

    fn drive(
        own: &mut OwnPlayer,
        world: &World,
        input: PlayerInput,
        mods: PhysicsMods,
        ticks: u32,
        msgs: &mut Vec<ClientMessage>,
    ) {
        for _ in 0..ticks {
            msgs.extend(own.update(world, input, DT, false, mods));
        }
    }

    /// §8 / protocol.rs `anim::AIR_JUMPED`: a Gust Jar mid-air jump
    /// consumed by the shared step raises the flag on exactly one
    /// `PlayerState` (the next send), then clears.
    #[test]
    fn air_jump_flags_exactly_one_player_state() {
        let world = flat_world();
        let mods = PhysicsMods {
            extra_air_jumps: 1,
            ..PhysicsMods::NONE
        };
        let mut own = OwnPlayer::at(PlayerPhysics::from_feet(32.0, 40.0));
        let jump = PlayerInput {
            jump: true,
            ..PlayerInput::default()
        };
        let idle = PlayerInput::default();
        let mut msgs = Vec::new();

        // Settle onto the floor first (`on_ground` resolves on stepping).
        drive(&mut own, &world, idle, mods, 5, &mut msgs);
        assert!(own.phys.on_ground, "settled");
        // Ground jump, then release so the next press has an edge.
        drive(&mut own, &world, jump, mods, 10, &mut msgs);
        assert!(!own.phys.on_ground, "airborne after the ground jump");
        drive(&mut own, &world, idle, mods, 6, &mut msgs);
        assert_eq!(own.phys.air_jumps_used, 0);
        msgs.clear();

        // Mid-air press: the step consumes the air jump...
        drive(&mut own, &world, jump, mods, 6, &mut msgs);
        assert_eq!(own.phys.air_jumps_used, 1, "air jump consumed");
        // ...and the flag rides exactly one of the following states.
        drive(&mut own, &world, jump, mods, 30, &mut msgs);
        let flagged = msgs
            .iter()
            .filter(|m| {
                matches!(m, ClientMessage::PlayerState { anim, .. }
                    if anim & anim::AIR_JUMPED != 0)
            })
            .count();
        assert!(msgs.len() > 1, "several states sent while airborne");
        assert_eq!(flagged, 1, "AIR_JUMPED is one-shot");
    }

    /// `PlayerKnockback` adds to the predicted velocity (the server can't
    /// move a client-authoritative body) and cancels a held-jump rise.
    #[test]
    fn knockback_adds_to_velocity() {
        let mut own = OwnPlayer::at(PlayerPhysics::from_feet(32.0, 40.0));
        own.phys.vel = (2.0, -1.0);
        own.phys.jump_hold_left = 0.1;
        own.apply_knockback(-8.0, -4.0);
        assert_eq!(own.phys.vel, (-6.0, -5.0));
        assert_eq!(own.phys.jump_hold_left, 0.0);
    }
}
