//! Players: own-player prediction (fixed 60 Hz accumulator over the shared
//! physics step, decoupled from render FPS) and remote-player snapshot
//! interpolation (~100 ms behind, per ARCHITECTURE.md "Authority model").

use std::collections::VecDeque;

use ferraria_shared::items::ItemId;
use ferraria_shared::physics::{
    step_player_with_mods, PhysicsMods, PlayerInput, PlayerPhysics, StepResult,
};
use ferraria_shared::protocol::{anim, ClientMessage};
use ferraria_shared::{DT, SNAPSHOT_INTERVAL_TICKS};

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
            self.last_step = step_player_with_mods(world, &mut self.phys, input, DT, mods);
            self.tick += 1;
            if self.tick.is_multiple_of(SNAPSHOT_INTERVAL_TICKS as u64) {
                let state = ClientMessage::PlayerState {
                    pos: self.phys.pos,
                    vel: self.phys.vel,
                    facing: self.facing,
                    anim: self.anim_flags(),
                };
                if self.last_sent.as_ref() != Some(&state) {
                    self.last_sent = Some(state.clone());
                    out.push(state);
                }
            }
        }
        out
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
            snaps,
        }
    }

    pub fn push(&mut self, snap: Snapshot) {
        if self.snaps.len() >= SNAPSHOT_BUFFER {
            self.snaps.pop_front();
        }
        self.snaps.push_back(snap);
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
