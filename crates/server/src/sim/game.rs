//! The simulation task: a single tokio task that owns the authoritative
//! [`World`] and all player state, ticking at 60 tps (ARCHITECTURE.md
//! "Simulation" — no locks in game logic).
//!
//! Sessions talk to the sim through an mpsc of [`SimCommand`]; the sim talks
//! back through one bounded per-session channel of encoded frames
//! ([`Frame`], an `Arc` so broadcasts encode once). Sends from the sim are
//! always `try_send`: a session whose outbound queue is full
//! ([`OUTBOUND_QUEUE_FRAMES`]) gets dropped rather than ever blocking the
//! sim.
//!
//! [`Sim::handle_message`] is the intent dispatch point — mining, combat,
//! and inventory intents from later PRs each become one new match arm there.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{mpsc, oneshot};

use ferraria_shared::items::{inventory, InvSlot, ItemId, STARTING_KIT};
use ferraria_shared::physics::{PlayerPhysics, PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::{encode, AuthToken, ClientMessage, ServerMessage};
use ferraria_shared::rng::Pcg32;
use ferraria_shared::tiles::Tile;
use ferraria_shared::world::{
    World, CHUNK_SIZE, CHUNK_SUB_HYSTERESIS, CHUNK_SUB_RADIUS_X, CHUNK_SUB_RADIUS_Y, DAY_TICKS,
};
use ferraria_shared::{
    CHAT_MAX_CHARS, MAX_NAME_CHARS, MAX_PLAYERS, MAX_PLAYER_SPEED, MAX_TELEPORT_BUDGET_TILES,
    MAX_TELEPORT_PER_TICK, SNAPSHOT_INTERVAL_TICKS, TICK_RATE, TIME_SYNC_INTERVAL_TICKS,
};

/// One encoded `ServerMessage`, shared between sessions without re-encoding.
pub type Frame = Arc<[u8]>;

/// Per-session outbound queue capacity. A session that lets this many frames
/// pile up (dead/slow connection) is dropped — the sim never blocks.
pub const OUTBOUND_QUEUE_FRAMES: usize = 4096;

/// Session → sim command queue capacity (shared by all sessions; sessions
/// `send().await` so a hot client backpressures its own socket only).
pub const COMMAND_QUEUE: usize = 1024;

/// Warn when the tick loop runs this far behind real time.
const LAG_WARN_THRESHOLD: Duration = Duration::from_millis(250);
/// ... at most once per this interval.
const LAG_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// What sessions send the sim.
///
/// `epoch` is the per-session generation the sim minted in the `Join` reply.
/// Player ids are reused across reconnects (token reclaim keeps the id), so
/// a kicked session's late `Message`/`Disconnect` must not be honored once a
/// successor session owns the same id — the sim drops commands whose epoch
/// doesn't match the current occupant's.
pub enum SimCommand {
    /// Handshake (after the session validated `PROTOCOL_VERSION`). On
    /// success the sim queues Welcome + join state on `tx` and replies
    /// `Ok((player_id, epoch))`; on failure it replies `Err(reason)` and the
    /// session sends the `Reject`.
    Join {
        name: String,
        token: Option<AuthToken>,
        tx: mpsc::Sender<Frame>,
        reply: oneshot::Sender<Result<(u32, u64), String>>,
    },
    /// A decoded in-game message from a connected player.
    Message {
        player_id: u32,
        epoch: u64,
        msg: ClientMessage,
    },
    /// The session's socket closed.
    Disconnect { player_id: u32, epoch: u64 },
}

/// A connected player.
struct Player {
    name: String,
    token: AuthToken,
    /// Session generation; commands carrying a different epoch are stale
    /// (see [`SimCommand`]).
    epoch: u64,
    /// Top-left of the AABB, tile units (the `PlayerState` convention).
    pos: (f32, f32),
    vel: (f32, f32),
    facing: i8,
    anim: u8,
    /// Movement changed since the last snapshot broadcast.
    moved: bool,
    held_slot: u8,
    /// Flat §8 layout (`items::inventory`), server-authoritative.
    inventory: Vec<Option<InvSlot>>,
    /// Chunks this session currently receives ([`ServerMessage::ChunkData`]
    /// sent on subscribe; tile deltas while subscribed).
    chunks: HashSet<(u32, u32)>,
    /// Sim tick when the last `PlayerState` was processed (replenishes
    /// `move_budget`).
    last_state_tick: u64,
    /// Remaining displacement allowance for the teleport clamp, in tiles.
    /// Refills [`MAX_TELEPORT_PER_TICK`] per elapsed tick, capped at
    /// [`MAX_TELEPORT_BUDGET_TILES`]; every accepted `PlayerState` consumes
    /// its actual distance, so stacking messages within one tick cannot
    /// stack fresh clamp budgets.
    move_budget: f32,
    tx: mpsc::Sender<Frame>,
}

impl Player {
    fn center(&self) -> (f32, f32) {
        (
            self.pos.0 + PLAYER_WIDTH / 2.0,
            self.pos.1 + PLAYER_HEIGHT / 2.0,
        )
    }

    fn held_item(&self) -> Option<ItemId> {
        self.inventory
            .get(self.held_slot as usize)
            .copied()
            .flatten()
            .map(|s| s.item)
    }
}

/// State retained for a disconnected player; `name` + `token` in a later
/// `Hello` reclaims it (and later persists to disk, ARCHITECTURE.md
/// "Persistence").
struct OfflinePlayer {
    id: u32,
    token: AuthToken,
    pos: (f32, f32),
    held_slot: u8,
    inventory: Vec<Option<InvSlot>>,
}

/// The authoritative game state, owned by [`run`]'s task.
pub struct Sim {
    world: World,
    tick: u64,
    players: HashMap<u32, Player>,
    offline: HashMap<String, OfflinePlayer>,
    next_player_id: u32,
    /// Monotonic session-generation counter (player ids are reused across
    /// reconnects; epochs never are).
    next_epoch: u64,
    /// Encoded `ChunkData` frames; invalidated by [`Sim::change_tile`].
    chunk_cache: HashMap<(u32, u32), Frame>,
    /// Fallback token entropy when /dev/urandom is unavailable.
    token_rng: Pcg32,
    /// Players whose channel filled/closed mid-broadcast; removed at the next
    /// flush point (can't remove while iterating).
    pending_kicks: Vec<u32>,
    /// Mirror of `players.len()` for the /api/status handler.
    player_count: Arc<AtomicUsize>,
}

impl Sim {
    pub fn new(world: World, player_count: Arc<AtomicUsize>) -> Sim {
        Sim {
            world,
            tick: 0,
            players: HashMap::new(),
            offline: HashMap::new(),
            next_player_id: 1,
            next_epoch: 1,
            chunk_cache: HashMap::new(),
            token_rng: Pcg32::new(entropy_seed()),
            pending_kicks: Vec::new(),
            player_count,
        }
    }

    /// Applies one session command. Public so tests can drive the sim
    /// without the tokio interval.
    pub fn handle(&mut self, cmd: SimCommand) {
        match cmd {
            SimCommand::Join {
                name,
                token,
                tx,
                reply,
            } => self.join(name, token, tx, reply),
            SimCommand::Message {
                player_id,
                epoch,
                msg,
            } => self.handle_message(player_id, epoch, msg),
            SimCommand::Disconnect { player_id, epoch } => self.disconnect(player_id, epoch),
        }
        self.flush_kicks();
    }

    /// Advances the world one tick (60/s).
    pub fn tick(&mut self) {
        self.tick += 1;

        // Time of day (§9): 1 tick of `time` per sim tick.
        self.world.time += 1;
        if self.world.time >= DAY_TICKS {
            self.world.time = 0;
            self.world.day += 1;
        }
        if self.tick.is_multiple_of(TIME_SYNC_INTERVAL_TICKS as u64) {
            self.broadcast(&ServerMessage::TimeSync {
                time: self.world.time,
                day: self.world.day,
            });
        }

        // Movement snapshots every 3 ticks (20/s), only for players that
        // actually moved; chunk subscriptions follow accepted movement.
        if self.tick.is_multiple_of(SNAPSHOT_INTERVAL_TICKS as u64) {
            let movers: Vec<u32> = self
                .players
                .iter()
                .filter(|(_, p)| p.moved)
                .map(|(&id, _)| id)
                .collect();
            for id in movers {
                if let Some(p) = self.players.get_mut(&id) {
                    p.moved = false;
                    let frame: Frame = encode(&ServerMessage::PlayerMoved {
                        id,
                        pos: p.pos,
                        vel: p.vel,
                        facing: p.facing,
                        anim: p.anim,
                    })
                    .into();
                    self.broadcast_frame(&frame, Some(id));
                    self.update_player_chunks(id);
                }
            }
        }

        // (Later PRs: enemies, fluids, item drops, NPCs tick here.)

        self.flush_kicks();
    }

    // ---- Join / leave -------------------------------------------------------

    fn join(
        &mut self,
        raw_name: String,
        token: Option<AuthToken>,
        tx: mpsc::Sender<Frame>,
        reply: oneshot::Sender<Result<(u32, u64), String>>,
    ) {
        let Some(name) = sanitize_name(&raw_name) else {
            let _ = reply.send(Err(format!(
                "invalid name (1..={MAX_NAME_CHARS} printable characters)"
            )));
            return;
        };
        if self.players.len() >= MAX_PLAYERS {
            let _ = reply.send(Err(format!("server is full ({MAX_PLAYERS} players)")));
            return;
        }
        if self.players.values().any(|p| p.name == name) {
            let _ = reply.send(Err(format!("a player named \"{name}\" is already online")));
            return;
        }
        // Reclaim a previous identity (name + matching token) or mint a new
        // one. A known name with a wrong/missing token is protected.
        let reclaimed = match self.offline.get(&name) {
            Some(rec) if token == Some(rec.token) => self.offline.remove(&name),
            Some(_) => {
                let _ = reply.send(Err(format!(
                    "the name \"{name}\" belongs to another player on this server"
                )));
                return;
            }
            None => None,
        };
        let (id, token, pos, held_slot, inv) = match reclaimed {
            Some(rec) => (rec.id, rec.token, rec.pos, rec.held_slot, rec.inventory),
            None => {
                let id = self.next_player_id;
                self.next_player_id += 1;
                let pos = spawn_pos(&self.world);
                (id, self.fresh_token(), pos, 0, starting_inventory())
            }
        };
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        let player = Player {
            name: name.clone(),
            token,
            epoch,
            pos,
            vel: (0.0, 0.0),
            facing: 1,
            anim: 0,
            moved: false,
            held_slot,
            inventory: inv,
            chunks: HashSet::new(),
            last_state_tick: self.tick,
            // One tick of allowance until the first state replenishes it.
            move_budget: MAX_TELEPORT_PER_TICK,
            tx,
        };

        // Announce to everyone already here (the newcomer isn't in the map
        // yet, so no self-send).
        self.broadcast(&ServerMessage::PlayerJoined {
            id,
            name: name.clone(),
            pos,
        });
        self.broadcast(&ServerMessage::PlayerHeldItem {
            id,
            slot: held_slot,
            item: player.held_item(),
        });

        // Queue the newcomer's join state in order: Welcome first, then the
        // player's own authoritative position, inventory, time, the
        // existing-player roster, then chunks.
        //
        // The own-id `PlayerMoved` matters on token reclaim: `Welcome`
        // carries only the world spawn, but a reclaim restores the player's
        // previous position (and the chunk window streams around it).
        // Without this frame the client would predict from the spawn it
        // placed itself at — frozen forever on a far reclaim because the
        // spawn chunk never streams. The client snaps to any own-id
        // `PlayerMoved` that disagrees by > 1 tile, so this is a no-op on a
        // fresh spawn (identical placement formula on both sides).
        let mut frames: Vec<Frame> = vec![
            encode(&ServerMessage::Welcome {
                player_id: id,
                token,
                world_width: self.world.width,
                world_height: self.world.height,
                spawn: self.world.spawn,
                time: self.world.time,
                day: self.world.day,
                flags: self.world.flags,
            })
            .into(),
            encode(&ServerMessage::PlayerMoved {
                id,
                pos,
                vel: (0.0, 0.0),
                facing: 1,
                anim: 0,
            })
            .into(),
            encode(&ServerMessage::InventorySync {
                slots: player.inventory.clone(),
            })
            .into(),
            encode(&ServerMessage::TimeSync {
                time: self.world.time,
                day: self.world.day,
            })
            .into(),
        ];
        for (&oid, other) in &self.players {
            frames.push(
                encode(&ServerMessage::PlayerJoined {
                    id: oid,
                    name: other.name.clone(),
                    pos: other.pos,
                })
                .into(),
            );
            frames.push(
                encode(&ServerMessage::PlayerHeldItem {
                    id: oid,
                    slot: other.held_slot,
                    item: other.held_item(),
                })
                .into(),
            );
        }
        for frame in frames {
            // Can only fail if the session already died; Disconnect follows.
            let _ = player.tx.try_send(frame);
        }

        self.players.insert(id, player);
        self.player_count
            .store(self.players.len(), Ordering::Relaxed);
        self.update_player_chunks(id);
        let _ = reply.send(Ok((id, epoch)));
        tracing::info!(player = id, name = %name, "player joined");
    }

    /// A session's socket closed. Only removes the player when `epoch`
    /// still matches: a kicked session's late Disconnect must not boot the
    /// successor session that reclaimed the same id in the meantime.
    fn disconnect(&mut self, id: u32, epoch: u64) {
        if self.players.get(&id).is_some_and(|p| p.epoch == epoch) {
            self.remove_player(id);
        }
    }

    /// Removes a player, parks their state for re-join, tells everyone else.
    fn remove_player(&mut self, id: u32) {
        let Some(p) = self.players.remove(&id) else {
            return; // disconnect raced a kick — already gone
        };
        tracing::info!(player = id, name = %p.name, "player left");
        self.offline.insert(
            p.name,
            OfflinePlayer {
                id,
                token: p.token,
                pos: p.pos,
                held_slot: p.held_slot,
                inventory: p.inventory,
            },
        );
        self.player_count
            .store(self.players.len(), Ordering::Relaxed);
        self.broadcast(&ServerMessage::PlayerLeft { id });
    }

    fn flush_kicks(&mut self) {
        while let Some(id) = self.pending_kicks.pop() {
            if self.players.contains_key(&id) {
                tracing::warn!(
                    player = id,
                    "dropping session: outbound queue full or closed"
                );
                self.remove_player(id);
            }
        }
    }

    // ---- Intent dispatch ----------------------------------------------------

    /// THE dispatch point for client messages. Later feature PRs (mining,
    /// placing, combat, inventory, NPCs) add one arm per intent here.
    fn handle_message(&mut self, id: u32, epoch: u64, msg: ClientMessage) {
        if self.players.get(&id).is_none_or(|p| p.epoch != epoch) {
            return; // message raced a disconnect, or is from a stale session
        }
        match msg {
            // The session layer consumed the real Hello; a duplicate is a
            // confused client.
            ClientMessage::Hello { .. } => {
                tracing::debug!(player = id, "duplicate Hello ignored");
            }
            ClientMessage::Ping { nonce } => self.send_to(id, &ServerMessage::Pong { nonce }),
            ClientMessage::PlayerState {
                pos,
                vel,
                facing,
                anim,
            } => self.player_state(id, pos, vel, facing, anim),
            ClientMessage::SelectSlot { slot } => self.select_slot(id, slot),
            ClientMessage::Chat { text } => self.chat(id, text),
            other => {
                tracing::debug!(player = id, msg = ?other, "intent not implemented yet");
            }
        }
    }

    /// Client-authoritative movement intake with sanity clamps
    /// (ARCHITECTURE.md "Authority model"). Displacement draws from a
    /// banked per-player budget rather than `ticks since last state`, so N
    /// `PlayerState`s processed within one tick share one allowance instead
    /// of each minting a fresh [`MAX_TELEPORT_PER_TICK`].
    fn player_state(&mut self, id: u32, pos: (f32, f32), vel: (f32, f32), facing: i8, anim: u8) {
        let (w, h) = (self.world.width as f32, self.world.height as f32);
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let ticks_elapsed = self.tick.saturating_sub(p.last_state_tick);
        p.last_state_tick = self.tick;
        p.move_budget = (p.move_budget + ticks_elapsed as f32 * MAX_TELEPORT_PER_TICK)
            .min(MAX_TELEPORT_BUDGET_TILES);

        let finite =
            pos.0.is_finite() && pos.1.is_finite() && vel.0.is_finite() && vel.1.is_finite();
        let (dx, dy) = (pos.0 - p.pos.0, pos.1 - p.pos.1);
        if !finite || dx * dx + dy * dy > p.move_budget * p.move_budget {
            // Teleport/garbage: keep the server position and snap the client
            // back with an authoritative own-id correction.
            tracing::debug!(player = id, ?pos, "rejected player state; snapping back");
            let correction = ServerMessage::PlayerMoved {
                id,
                pos: p.pos,
                vel: (0.0, 0.0),
                facing: p.facing,
                anim: p.anim,
            };
            self.send_to(id, &correction);
            return;
        }
        p.move_budget -= (dx * dx + dy * dy).sqrt();
        p.pos = (
            pos.0.clamp(0.0, (w - PLAYER_WIDTH).max(0.0)),
            pos.1.clamp(0.0, (h - PLAYER_HEIGHT).max(0.0)),
        );
        p.vel = (
            vel.0.clamp(-MAX_PLAYER_SPEED, MAX_PLAYER_SPEED),
            vel.1.clamp(-MAX_PLAYER_SPEED, MAX_PLAYER_SPEED),
        );
        p.facing = if facing < 0 { -1 } else { 1 };
        p.anim = anim;
        p.moved = true;
    }

    fn select_slot(&mut self, id: u32, slot: u8) {
        if slot as usize >= inventory::HOTBAR {
            return;
        }
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        p.held_slot = slot;
        let frame: Frame = encode(&ServerMessage::PlayerHeldItem {
            id,
            slot,
            item: p.held_item(),
        })
        .into();
        // Remote clients render the held item; the owner already knows.
        self.broadcast_frame(&frame, Some(id));
    }

    fn chat(&mut self, id: u32, text: String) {
        let Some(p) = self.players.get(&id) else {
            return;
        };
        let Some(text) = sanitize_chat(&text) else {
            return; // empty after sanitizing
        };
        let from = p.name.clone();
        // Everyone including the sender (echo doubles as delivery receipt).
        self.broadcast(&ServerMessage::Chat { from, text });
    }

    // ---- World mutation -------------------------------------------------------

    /// The single tile-mutation point: writes the tile, invalidates the
    /// chunk cache, and pushes the delta to every subscribed player. All
    /// future mining/placing/door/growth code must change tiles through
    /// here so caches and clients can never go stale.
    pub fn change_tile(&mut self, x: u32, y: u32, tile: Tile) {
        if !self.world.in_bounds(x, y) {
            return;
        }
        self.world.set_tile(x, y, tile);
        let chunk = (x / CHUNK_SIZE, y / CHUNK_SIZE);
        self.chunk_cache.remove(&chunk);
        let frame: Frame = encode(&ServerMessage::TileChanged { x, y, tile }).into();
        let subscribed: Vec<u32> = self
            .players
            .iter()
            .filter(|(_, p)| p.chunks.contains(&chunk))
            .map(|(&id, _)| id)
            .collect();
        for id in subscribed {
            self.send_frame_to(id, frame.clone());
        }
    }

    /// Read access for tests/handlers (mutate via [`Sim::change_tile`]).
    pub fn world(&self) -> &World {
        &self.world
    }

    // ---- Chunk streaming ------------------------------------------------------

    /// Sends newly entered chunks and drops far ones, with hysteresis: a
    /// chunk is subscribed inside the 5×3 window but only unsubscribed once
    /// outside the (5+2)×(3+2) window.
    fn update_player_chunks(&mut self, id: u32) {
        let (missing, keep_x, keep_y) = {
            let Some(p) = self.players.get(&id) else {
                return;
            };
            let center = player_chunk(&self.world, p.center());
            let (want_x, want_y) =
                chunk_window(&self.world, center, CHUNK_SUB_RADIUS_X, CHUNK_SUB_RADIUS_Y);
            let keep = chunk_window(
                &self.world,
                center,
                CHUNK_SUB_RADIUS_X + CHUNK_SUB_HYSTERESIS,
                CHUNK_SUB_RADIUS_Y + CHUNK_SUB_HYSTERESIS,
            );
            let mut missing = Vec::new();
            for cy in want_y {
                for cx in want_x.clone() {
                    if !p.chunks.contains(&(cx, cy)) {
                        missing.push((cx, cy));
                    }
                }
            }
            (missing, keep.0, keep.1)
        };
        let frames: Vec<((u32, u32), Frame)> = missing
            .into_iter()
            .map(|c| (c, self.chunk_frame(c.0, c.1)))
            .collect();
        let mut dead = false;
        if let Some(p) = self.players.get_mut(&id) {
            for (c, frame) in frames {
                if p.tx.try_send(frame).is_ok() {
                    p.chunks.insert(c);
                } else {
                    dead = true;
                    break;
                }
            }
            p.chunks
                .retain(|&(cx, cy)| keep_x.contains(&cx) && keep_y.contains(&cy));
        }
        if dead {
            self.pending_kicks.push(id);
        }
    }

    /// Encoded `ChunkData` frame, cached until [`Sim::change_tile`]
    /// invalidates it.
    fn chunk_frame(&mut self, cx: u32, cy: u32) -> Frame {
        if let Some(frame) = self.chunk_cache.get(&(cx, cy)) {
            return frame.clone();
        }
        let frame: Frame = encode(&ServerMessage::ChunkData {
            cx,
            cy,
            bytes: self.world.encode_chunk(cx, cy),
        })
        .into();
        self.chunk_cache.insert((cx, cy), frame.clone());
        frame
    }

    // ---- Outbound helpers -------------------------------------------------------

    fn send_to(&mut self, id: u32, msg: &ServerMessage) {
        self.send_frame_to(id, encode(msg).into());
    }

    fn send_frame_to(&mut self, id: u32, frame: Frame) {
        if let Some(p) = self.players.get(&id) {
            if p.tx.try_send(frame).is_err() {
                self.pending_kicks.push(id);
            }
        }
    }

    fn broadcast(&mut self, msg: &ServerMessage) {
        let frame: Frame = encode(msg).into();
        self.broadcast_frame(&frame, None);
    }

    fn broadcast_frame(&mut self, frame: &Frame, except: Option<u32>) {
        let mut dead = Vec::new();
        for (&id, p) in &self.players {
            if Some(id) == except {
                continue;
            }
            if p.tx.try_send(frame.clone()).is_err() {
                dead.push(id);
            }
        }
        self.pending_kicks.extend(dead);
    }

    fn fresh_token(&mut self) -> AuthToken {
        let mut token = [0u8; 16];
        // /dev/urandom directly (no getrandom crate, docs/NETWORKING.md);
        // fall back to the entropy-seeded PRNG where it doesn't exist.
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut token).is_ok() {
                return token;
            }
        }
        token[..8].copy_from_slice(&self.token_rng.next_u64().to_le_bytes());
        token[8..].copy_from_slice(&self.token_rng.next_u64().to_le_bytes());
        token
    }
}

/// Drives the sim at a fixed 60 tps. `Burst` catch-up: after a stall the
/// interval fires back-to-back until the schedule is caught up, and we log
/// (rate-limited) whenever the loop falls noticeably behind.
pub async fn run(mut sim: Sim, mut rx: mpsc::Receiver<SimCommand>) {
    let period = Duration::from_secs_f64(1.0 / TICK_RATE as f64);
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let started = Instant::now();
    let mut ticks: u64 = 0;
    let mut last_lag_warn: Option<Instant> = None;
    loop {
        interval.tick().await;
        let scheduled = Duration::from_secs_f64(ticks as f64 / TICK_RATE as f64);
        let behind = started.elapsed().saturating_sub(scheduled);
        if behind > LAG_WARN_THRESHOLD
            && last_lag_warn.is_none_or(|t| t.elapsed() >= LAG_WARN_INTERVAL)
        {
            tracing::warn!(
                behind_ms = behind.as_millis() as u64,
                tick = ticks,
                "sim ticks falling behind; bursting to catch up"
            );
            last_lag_warn = Some(Instant::now());
        }
        ticks += 1;

        loop {
            match rx.try_recv() {
                Ok(cmd) => sim.handle(cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    tracing::info!("command channel closed; sim task stopping");
                    return;
                }
            }
        }
        sim.tick();
    }
}

// ---- Pure helpers (unit-tested below) ----------------------------------------

/// Initial AABB top-left for a player standing on the world spawn platform.
fn spawn_pos(world: &World) -> (f32, f32) {
    // `spawn` is the air tile whose row below is the platform (worldgen pass
    // 12): feet rest on top of row `spawn.1 + 1`.
    PlayerPhysics::from_feet(world.spawn.0 as f32 + 0.5, (world.spawn.1 + 1) as f32).pos
}

/// Fresh §8 inventory: the starting kit in the first hotbar slots.
fn starting_inventory() -> Vec<Option<InvSlot>> {
    let mut slots = vec![None; inventory::TOTAL];
    for (slot, kit) in slots.iter_mut().zip(STARTING_KIT) {
        *slot = Some(kit);
    }
    slots
}

/// Strips control characters, trims, caps at [`CHAT_MAX_CHARS`] characters.
/// `None` if nothing printable remains.
fn sanitize_chat(text: &str) -> Option<String> {
    let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    let capped: String = cleaned.trim().chars().take(CHAT_MAX_CHARS).collect();
    (!capped.is_empty()).then_some(capped)
}

/// Like chat sanitizing but bounded by [`MAX_NAME_CHARS`]; over-long names
/// are rejected rather than truncated (the name is the player's identity
/// key).
fn sanitize_name(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    let len = trimmed.chars().count();
    (1..=MAX_NAME_CHARS)
        .contains(&len)
        .then(|| trimmed.to_string())
}

/// Chunk containing a world-space point, clamped into the chunk grid.
fn player_chunk(world: &World, point: (f32, f32)) -> (u32, u32) {
    (
        ((point.0.max(0.0) as u32) / CHUNK_SIZE).min(world.chunks_x() - 1),
        ((point.1.max(0.0) as u32) / CHUNK_SIZE).min(world.chunks_y() - 1),
    )
}

/// The chunk-coordinate window `center ± (rx, ry)`, clamped to the grid.
fn chunk_window(world: &World, center: (u32, u32), rx: u32, ry: u32) -> (Range<u32>, Range<u32>) {
    (
        center.0.saturating_sub(rx)..(center.0 + rx + 1).min(world.chunks_x()),
        center.1.saturating_sub(ry)..(center.1 + ry + 1).min(world.chunks_y()),
    )
}

/// 64 bits of process entropy without the getrandom crate: `RandomState`'s
/// per-instance keys come from OS entropy, mixed with the wall clock.
fn entropy_seed() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    hasher.write_u64(nanos);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferraria_shared::protocol::decode;
    use ferraria_shared::world::NEW_WORLD_TIME;

    /// A sim over a small empty world with an inspectable player counter.
    fn test_sim() -> Sim {
        let mut world = World::new(320, 320);
        world.spawn = (160, 100);
        Sim::new(world, Arc::new(AtomicUsize::new(0)))
    }

    /// Joins a player, returning their id, session epoch, and the outbound
    /// frame receiver.
    fn join(
        sim: &mut Sim,
        name: &str,
        token: Option<AuthToken>,
    ) -> (u32, u64, mpsc::Receiver<Frame>) {
        let (reply, rx) = try_join(sim, name, token);
        let (id, epoch) = reply.expect("join accepted");
        (id, epoch, rx)
    }

    fn try_join(
        sim: &mut Sim,
        name: &str,
        token: Option<AuthToken>,
    ) -> (Result<(u32, u64), String>, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_FRAMES);
        let (reply_tx, mut reply_rx) = oneshot::channel();
        sim.handle(SimCommand::Join {
            name: name.into(),
            token,
            tx,
            reply: reply_tx,
        });
        (reply_rx.try_recv().expect("sim replied"), rx)
    }

    /// Shorthand for an in-game message from a live session.
    fn msg(sim: &mut Sim, player_id: u32, epoch: u64, msg: ClientMessage) {
        sim.handle(SimCommand::Message {
            player_id,
            epoch,
            msg,
        });
    }

    fn drain(rx: &mut mpsc::Receiver<Frame>) -> Vec<ServerMessage> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(decode::<ServerMessage>(&frame).expect("valid frame"));
        }
        out
    }

    #[test]
    fn join_sends_welcome_inventory_time_and_chunks() {
        let mut sim = test_sim();
        let (id, _epoch, mut rx) = join(&mut sim, "alice", None);
        let msgs = drain(&mut rx);
        let ServerMessage::Welcome {
            player_id,
            spawn,
            time,
            ..
        } = &msgs[0]
        else {
            panic!("first frame must be Welcome, got {:?}", msgs[0]);
        };
        assert_eq!(*player_id, id);
        assert_eq!(*spawn, (160, 100));
        assert_eq!(*time, NEW_WORLD_TIME);
        // Authoritative own placement directly after Welcome (the client
        // adopts it on reconnect reclaim).
        let ServerMessage::PlayerMoved {
            id: moved_id, pos, ..
        } = &msgs[1]
        else {
            panic!("second frame must be the own-id placement: {:?}", msgs[1]);
        };
        assert_eq!(*moved_id, id);
        assert_eq!(*pos, sim.players[&id].pos);
        let inv = msgs.iter().find_map(|m| match m {
            ServerMessage::InventorySync { slots } => Some(slots.clone()),
            _ => None,
        });
        let inv = inv.expect("InventorySync on join");
        assert_eq!(inv.len(), inventory::TOTAL);
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::WoodSword, 1)));
        assert_eq!(inv[3], Some(InvSlot::new(ItemId::Torch, 5)));
        assert!(msgs
            .iter()
            .any(|m| matches!(m, ServerMessage::TimeSync { .. })));
        // 5×3 window around chunk (2, 1) of a 5×5 chunk grid: x 0..5, y 0..3.
        let chunks = msgs
            .iter()
            .filter(|m| matches!(m, ServerMessage::ChunkData { .. }))
            .count();
        assert_eq!(chunks, 15);
    }

    #[test]
    fn rejects_dupes_imposters_and_a_full_server() {
        let mut sim = test_sim();
        let (_, _, _rx) = join(&mut sim, "alice", None);
        let (dupe, _rx2) = try_join(&mut sim, "alice", None);
        assert!(dupe.is_err(), "duplicate name must be rejected");
        let (bad_name, _rx3) = try_join(&mut sim, "  \u{7} ", None);
        assert!(bad_name.is_err(), "unprintable name must be rejected");

        for i in 1..MAX_PLAYERS {
            let (r, rx) = try_join(&mut sim, &format!("p{i}"), None);
            r.expect("under the cap");
            std::mem::forget(rx); // keep channels open
        }
        assert_eq!(sim.players.len(), MAX_PLAYERS);
        let (full, _rx4) = try_join(&mut sim, "late", None);
        assert!(full.is_err(), "17th player must be rejected");
    }

    #[test]
    fn token_reclaims_identity_and_blocks_imposters() {
        let mut sim = test_sim();
        let (id, epoch, mut rx) = join(&mut sim, "alice", None);
        let msgs = drain(&mut rx);
        let &ServerMessage::Welcome { token, .. } = &msgs[0] else {
            panic!("welcome");
        };
        // Move alice somewhere, then disconnect.
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlayerState {
                pos: (165.0, 95.0),
                vel: (0.0, 0.0),
                facing: -1,
                anim: 0,
            },
        );
        sim.handle(SimCommand::Disconnect {
            player_id: id,
            epoch,
        });
        assert!(sim.players.is_empty());

        // Wrong token: rejected. Right token: same id, position restored.
        let (imposter, _rx) = try_join(&mut sim, "alice", Some([0xee; 16]));
        assert!(imposter.is_err());
        let (re_id, _, mut rx) = join(&mut sim, "alice", Some(token));
        assert_eq!(re_id, id, "identity reclaimed");
        let msgs = drain(&mut rx);
        assert!(msgs
            .iter()
            .any(|m| matches!(m, ServerMessage::Welcome { .. })));
        assert_eq!(sim.players[&re_id].pos, (165.0, 95.0));
        // The reclaimed position is pushed to the client as an own-id
        // correction right after Welcome — otherwise the client would
        // predict from the world spawn and desync (or freeze, far away).
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ServerMessage::PlayerMoved { id, pos, .. }
                    if *id == re_id && *pos == (165.0, 95.0))),
            "own-position frame in the reclaim join state: {msgs:?}"
        );
    }

    #[test]
    fn stale_session_commands_cannot_touch_a_reclaimed_session() {
        let mut sim = test_sim();
        let (id, old_epoch, mut rx) = join(&mut sim, "alice", None);
        let msgs = drain(&mut rx);
        let &ServerMessage::Welcome { token, .. } = &msgs[0] else {
            panic!("welcome");
        };
        // Session 1 is dropped (kick or disconnect) and alice reclaims her
        // identity with a new session reusing the same player id.
        sim.handle(SimCommand::Disconnect {
            player_id: id,
            epoch: old_epoch,
        });
        let (re_id, new_epoch, mut rx2) = join(&mut sim, "alice", Some(token));
        assert_eq!(re_id, id, "same id reused — the hazard under test");
        assert_ne!(old_epoch, new_epoch);

        // Session 1's pump finally ends and sends its (now stale)
        // Disconnect: the live session must survive.
        sim.handle(SimCommand::Disconnect {
            player_id: id,
            epoch: old_epoch,
        });
        assert!(
            sim.players.contains_key(&id),
            "stale Disconnect must not remove the reclaimed live session"
        );

        // Stale in-game messages are ignored too.
        drain(&mut rx2);
        msg(
            &mut sim,
            id,
            old_epoch,
            ClientMessage::Chat { text: "boo".into() },
        );
        assert!(
            drain(&mut rx2).is_empty(),
            "stale-session chat must be dropped"
        );

        // The live epoch still works normally.
        sim.handle(SimCommand::Disconnect {
            player_id: id,
            epoch: new_epoch,
        });
        assert!(sim.players.is_empty(), "live Disconnect still removes");
    }

    #[test]
    fn movement_is_clamped_and_rebroadcast_every_three_ticks() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        let (b, b_epoch, mut rx_b) = join(&mut sim, "bob", None);
        drain(&mut rx_a);
        drain(&mut rx_b);

        let start = sim.players[&b].pos;
        let target = (start.0 + 3.0, start.1 - 1.0);
        msg(
            &mut sim,
            b,
            b_epoch,
            ClientMessage::PlayerState {
                pos: target,
                vel: (60.0, 0.0), // over MAX_PLAYER_SPEED → clamped
                facing: 1,
                anim: 0,
            },
        );
        for _ in 0..SNAPSHOT_INTERVAL_TICKS {
            sim.tick();
        }
        let moved = drain(&mut rx_a)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::PlayerMoved { id, pos, vel, .. } if id == b => Some((pos, vel)),
                _ => None,
            })
            .expect("alice sees bob move");
        assert_eq!(moved.0, target);
        assert_eq!(moved.1 .0, MAX_PLAYER_SPEED);
        // The mover does not get their own snapshot.
        assert!(!drain(&mut rx_b)
            .iter()
            .any(|m| matches!(m, ServerMessage::PlayerMoved { id, .. } if *id == b)));

        // Teleporting far snaps back: position unchanged + own-id correction.
        msg(
            &mut sim,
            b,
            b_epoch,
            ClientMessage::PlayerState {
                pos: (target.0 + 200.0, target.1),
                vel: (0.0, 0.0),
                facing: 1,
                anim: 0,
            },
        );
        assert_eq!(sim.players[&b].pos, target, "teleport rejected");
        let correction = drain(&mut rx_b);
        assert!(
            correction
                .iter()
                .any(|m| matches!(m, ServerMessage::PlayerMoved { id, pos, .. }
                    if *id == b && *pos == target)),
            "snap-back correction sent to the offender: {correction:?}"
        );
        // NaN states are rejected too.
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::PlayerState {
                pos: (f32::NAN, 0.0),
                vel: (0.0, 0.0),
                facing: 1,
                anim: 0,
            },
        );
        assert!(sim.players[&a].pos.0.is_finite());
    }

    /// Regression: N `PlayerState`s arriving within one tick used to each
    /// mint a fresh MAX_TELEPORT_PER_TICK allowance (`ticks_elapsed.max(1)`
    /// after `last_state_tick` was refreshed mid-tick), letting a client hop
    /// N×30 tiles per tick. Stacked states now share one banked budget.
    #[test]
    fn same_tick_state_stacking_shares_one_teleport_budget() {
        let mut sim = test_sim();
        let (id, epoch, mut rx) = join(&mut sim, "alice", None);
        drain(&mut rx);
        let start = sim.players[&id].pos;

        // 5 stacked hops of 29 tiles each, no tick in between: only the
        // first fits the budget; the rest are rejected with corrections.
        for i in 1..=5 {
            msg(
                &mut sim,
                id,
                epoch,
                ClientMessage::PlayerState {
                    pos: (start.0 + 29.0 * i as f32, start.1),
                    vel: (0.0, 0.0),
                    facing: 1,
                    anim: 0,
                },
            );
        }
        let moved = sim.players[&id].pos.0 - start.0;
        assert!(
            moved <= MAX_TELEPORT_PER_TICK,
            "stacked states moved {moved} tiles in one tick"
        );
        let corrections = drain(&mut rx)
            .iter()
            .filter(|m| matches!(m, ServerMessage::PlayerMoved { id: mid, .. } if *mid == id))
            .count();
        assert_eq!(corrections, 4, "each rejected hop snaps the client back");

        // Budget replenishes with elapsed ticks: normal movement resumes.
        for _ in 0..SNAPSHOT_INTERVAL_TICKS {
            sim.tick();
        }
        let here = sim.players[&id].pos;
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlayerState {
                pos: (here.0 + 2.0, here.1),
                vel: (0.0, 0.0),
                facing: 1,
                anim: 0,
            },
        );
        assert_eq!(sim.players[&id].pos.0, here.0 + 2.0, "legit move accepted");
    }

    /// The banked budget is capped: going quiet for a long time must not
    /// bank a map-wide teleport.
    #[test]
    fn teleport_budget_is_capped_after_long_silence() {
        let mut world = World::new(1200, 320);
        world.spawn = (160, 100);
        let mut sim = Sim::new(world, Arc::new(AtomicUsize::new(0)));
        let (id, epoch, mut rx) = join(&mut sim, "alice", None);
        drain(&mut rx);
        // Bank far more ticks than the cap is worth.
        let cap_ticks = (MAX_TELEPORT_BUDGET_TILES / MAX_TELEPORT_PER_TICK) as u32;
        for _ in 0..cap_ticks * 10 {
            sim.tick();
        }
        let start = sim.players[&id].pos;
        msg(
            &mut sim,
            id,
            epoch,
            ClientMessage::PlayerState {
                pos: (start.0 + MAX_TELEPORT_BUDGET_TILES + 10.0, start.1),
                vel: (0.0, 0.0),
                facing: 1,
                anim: 0,
            },
        );
        assert_eq!(
            sim.players[&id].pos, start,
            "jump beyond the budget cap rejected even after long silence"
        );
    }

    #[test]
    fn select_slot_broadcasts_held_item() {
        let mut sim = test_sim();
        let (b, b_epoch, mut rx_b) = join(&mut sim, "bob", None);
        let (_a, _, mut rx_a) = join(&mut sim, "alice", None);
        drain(&mut rx_a);
        msg(&mut sim, b, b_epoch, ClientMessage::SelectSlot { slot: 1 });
        let held = drain(&mut rx_a)
            .into_iter()
            .find_map(|m| match m {
                ServerMessage::PlayerHeldItem { id, slot, item } if id == b => Some((slot, item)),
                _ => None,
            })
            .expect("held item broadcast");
        assert_eq!(held, (1, Some(ItemId::WoodPickaxe)));
        // Out-of-hotbar slots are ignored.
        msg(&mut sim, b, b_epoch, ClientMessage::SelectSlot { slot: 10 });
        assert_eq!(sim.players[&b].held_slot, 1);
        drain(&mut rx_b);
    }

    #[test]
    fn chat_is_sanitized_and_relayed() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        let (_b, _, mut rx_b) = join(&mut sim, "bob", None);
        drain(&mut rx_a);
        drain(&mut rx_b);
        let long = format!("  \u{1}hi\u{7} {}", "x".repeat(300));
        msg(&mut sim, a, a_epoch, ClientMessage::Chat { text: long });
        for rx in [&mut rx_a, &mut rx_b] {
            let chat = drain(rx)
                .into_iter()
                .find_map(|m| match m {
                    ServerMessage::Chat { from, text } => Some((from, text)),
                    _ => None,
                })
                .expect("chat relayed to all");
            assert_eq!(chat.0, "alice");
            assert!(chat.1.starts_with("hi x"));
            assert_eq!(chat.1.chars().count(), CHAT_MAX_CHARS);
        }
        // Whitespace-only chat is dropped.
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::Chat {
                text: "  \n ".into(),
            },
        );
        assert!(drain(&mut rx_b).is_empty());
    }

    #[test]
    fn ping_pong_and_time_advance() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        drain(&mut rx_a);
        msg(&mut sim, a, a_epoch, ClientMessage::Ping { nonce: 99 });
        assert!(drain(&mut rx_a)
            .iter()
            .any(|m| matches!(m, ServerMessage::Pong { nonce: 99 })));

        let t0 = sim.world.time;
        for _ in 0..TIME_SYNC_INTERVAL_TICKS {
            sim.tick();
        }
        assert_eq!(sim.world.time, t0 + TIME_SYNC_INTERVAL_TICKS);
        assert!(drain(&mut rx_a)
            .iter()
            .any(|m| matches!(m, ServerMessage::TimeSync { .. })));
    }

    #[test]
    fn day_wraps_and_increments() {
        let mut sim = test_sim();
        sim.world.time = DAY_TICKS - 1;
        sim.tick();
        assert_eq!(sim.world.time, 0);
        assert_eq!(sim.world.day, 1);
    }

    #[test]
    fn change_tile_invalidates_cache_and_notifies_subscribers() {
        use ferraria_shared::tiles::TileId;
        let mut sim = test_sim();
        let (_a, _, mut rx_a) = join(&mut sim, "alice", None);
        drain(&mut rx_a);
        // Spawn (160, 100) → chunk (2, 1) is subscribed.
        let (x, y) = (160, 100);
        let before = sim.chunk_frame(2, 1);
        sim.change_tile(x, y, Tile::of(TileId::Stone));
        let msgs = drain(&mut rx_a);
        assert!(msgs.iter().any(|m| matches!(m,
            ServerMessage::TileChanged { x: tx, y: ty, tile }
                if *tx == x && *ty == y && tile.id == TileId::Stone)));
        let after = sim.chunk_frame(2, 1);
        assert_ne!(before, after, "cached chunk frame was invalidated");
        // Unsubscribed chunk: no delta pushed.
        sim.change_tile(0, 319, Tile::of(TileId::Stone));
        assert!(drain(&mut rx_a).is_empty());
    }

    #[test]
    fn chunk_hysteresis_avoids_resubscribe_thrash() {
        let mut sim = test_sim();
        let (a, a_epoch, mut rx_a) = join(&mut sim, "alice", None);
        drain(&mut rx_a);
        let base = sim.players[&a].chunks.clone();
        // Bank a little movement budget, then step just over the chunk
        // border (x: ~160 → 193): the subscription window shifts right.
        for _ in 0..SNAPSHOT_INTERVAL_TICKS {
            sim.tick();
        }
        let pos = sim.players[&a].pos;
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::PlayerState {
                pos: (193.0, pos.1),
                vel: (0.0, 0.0),
                facing: 1,
                anim: 0,
            },
        );
        assert_eq!(sim.players[&a].pos.0, 193.0, "border step accepted");
        for _ in 0..SNAPSHOT_INTERVAL_TICKS {
            sim.tick();
        }
        let after = sim.players[&a].chunks.clone();
        // Hysteresis: the old left-edge chunks stay subscribed (within +1).
        assert!(after.is_superset(&base), "{base:?} -> {after:?}");
        // Walking back doesn't resend anything.
        msg(
            &mut sim,
            a,
            a_epoch,
            ClientMessage::PlayerState {
                pos,
                vel: (0.0, 0.0),
                facing: 1,
                anim: 0,
            },
        );
        drain(&mut rx_a);
        for _ in 0..SNAPSHOT_INTERVAL_TICKS {
            sim.tick();
        }
        assert!(!drain(&mut rx_a)
            .iter()
            .any(|m| matches!(m, ServerMessage::ChunkData { .. })));
    }

    #[test]
    fn full_outbound_queue_kicks_the_session() {
        let mut sim = test_sim();
        let (tx, _rx) = mpsc::channel(1); // tiny queue, receiver kept alive
        let (reply_tx, mut reply_rx) = oneshot::channel();
        sim.handle(SimCommand::Join {
            name: "slowpoke".into(),
            token: None,
            tx,
            reply: reply_tx,
        });
        // Join overflows the 1-frame queue immediately → kicked at flush.
        assert!(reply_rx.try_recv().is_ok());
        assert!(sim.players.is_empty(), "overflowing session was dropped");
        assert_eq!(sim.player_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn sanitizers() {
        assert_eq!(sanitize_chat("  hi  "), Some("hi".into()));
        assert_eq!(sanitize_chat("a\u{0}b\nc"), Some("abc".into()));
        assert_eq!(sanitize_chat(" \u{1b} \t "), None);
        assert_eq!(
            sanitize_chat(&"y".repeat(500)).map(|s| s.chars().count()),
            Some(CHAT_MAX_CHARS)
        );
        assert_eq!(sanitize_name(" moo "), Some("moo".into()));
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name(&"n".repeat(MAX_NAME_CHARS + 1)), None);
        assert!(sanitize_name(&"n".repeat(MAX_NAME_CHARS)).is_some());
    }

    #[test]
    fn fresh_tokens_differ() {
        let mut sim = test_sim();
        let a = sim.fresh_token();
        let b = sim.fresh_token();
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 16]);
    }
}
