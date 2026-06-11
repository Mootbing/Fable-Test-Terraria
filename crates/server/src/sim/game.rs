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

use ferraria_shared::coins::coin_total;
use ferraria_shared::inventory_ops::SlotOp;
use ferraria_shared::items::{inventory, InvSlot, ItemId, STARTING_KIT};
use ferraria_shared::physics::{PlayerPhysics, PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::{encode, AuthToken, ClientMessage, ServerMessage};
use ferraria_shared::rng::Pcg32;
use ferraria_shared::tiles::Tile;
use ferraria_shared::world::{
    World, CHUNK_SIZE, CHUNK_SUB_HYSTERESIS, CHUNK_SUB_RADIUS_X, CHUNK_SUB_RADIUS_Y, DAY_TICKS,
};
use ferraria_shared::{
    CHAT_MAX_CHARS, HELD_ITEM_BROADCAST_MIN_TICKS, MAX_NAME_CHARS, MAX_PLAYERS, MAX_PLAYER_SPEED,
    MAX_TELEPORT_BUDGET_TILES, MAX_TELEPORT_PER_TICK, PLAYER_BASE_MAX_HP, SNAPSHOT_INTERVAL_TICKS,
    TICK_RATE, TIME_SYNC_INTERVAL_TICKS,
};

use super::entities::EntityStore;
use super::fluids::FluidSim;
use super::interact::TileDamage;
use super::npc::TownState;

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

/// A connected player. `pub(crate)` so the sibling sim modules
/// (`interact`, `entities`, `inventory`, `world_tick`) can implement their
/// systems as further `impl Sim` blocks.
pub(crate) struct Player {
    pub(crate) name: String,
    token: AuthToken,
    /// Session generation; commands carrying a different epoch are stale
    /// (see [`SimCommand`]).
    epoch: u64,
    /// Top-left of the AABB, tile units (the `PlayerState` convention).
    pub(crate) pos: (f32, f32),
    pub(crate) vel: (f32, f32),
    pub(crate) facing: i8,
    pub(crate) anim: u8,
    /// Movement changed since the last snapshot broadcast.
    pub(crate) moved: bool,
    pub(crate) held_slot: u8,
    /// Flat §8 layout (`items::inventory`), server-authoritative.
    pub(crate) inventory: Vec<Option<InvSlot>>,
    /// Origin of the chest this player has open, if any (§11: a chest is
    /// locked to one open player; mirrored in [`Sim::chest_locks`]).
    pub(crate) open_chest: Option<(u32, u32)>,
    /// Chunks this session currently receives ([`ServerMessage::ChunkData`]
    /// sent on subscribe; tile deltas while subscribed).
    pub(crate) chunks: HashSet<(u32, u32)>,
    /// Sim tick when the last `PlayerState` was processed (replenishes
    /// `move_budget`).
    last_state_tick: u64,
    /// Remaining displacement allowance for the teleport clamp, in tiles.
    /// Refills [`MAX_TELEPORT_PER_TICK`] per elapsed tick, capped at
    /// [`MAX_TELEPORT_BUDGET_TILES`]; every accepted `PlayerState` consumes
    /// its actual distance, so stacking messages within one tick cannot
    /// stack fresh clamp budgets.
    move_budget: f32,
    /// Tick of the last accepted tool swing (`HitTile`/`HitWall` rate
    /// limiting, §2/§4.1); `None` until the first swing of the session.
    pub(crate) last_swing_tick: Option<u64>,
    /// Tick of the last accepted `ToggleDoor`
    /// ([`crate::sim::interact`]-enforced anti-amplification cooldown,
    /// [`ferraria_shared::DOOR_TOGGLE_COOLDOWN_TICKS`]).
    pub(crate) last_door_toggle_tick: Option<u64>,
    /// Tick of the last selection-driven `PlayerHeldItem` broadcast;
    /// `SelectSlot` floods coalesce against it
    /// ([`ferraria_shared::HELD_ITEM_BROADCAST_MIN_TICKS`]).
    last_held_broadcast_tick: Option<u64>,
    /// A coalesced selection broadcast is pending for this player.
    held_broadcast_dirty: bool,
    // ---- Survival/combat state (§8, sim::survival / sim::combat) ----------
    pub(crate) hp: u16,
    pub(crate) max_hp: u16,
    pub(crate) dead: bool,
    /// Tick the §8 respawn timer elapses (valid while `dead`).
    pub(crate) respawn_ready_tick: u64,
    /// §0 player i-frames: hits are ignored until this tick.
    pub(crate) iframe_until: u64,
    pub(crate) last_damage_tick: u64,
    /// Fractional accumulators: passive regen, Burning DPS, drowning DPS.
    pub(crate) regen_acc: f32,
    pub(crate) burn_acc: f32,
    pub(crate) drown_acc: f32,
    /// §8 breath units (0..=PLAYER_MAX_BREATH).
    pub(crate) breath: u16,
    /// Active timed debuffs: (kind, remaining ticks).
    pub(crate) debuffs: Vec<(ferraria_shared::protocol::Debuff, u32)>,
    /// Server-observed fall tracking (see `sim::survival` module docs).
    pub(crate) fall_accum: f32,
    pub(crate) was_grounded: bool,
    /// Personal bed spawn (§8), the bed's multitile origin.
    pub(crate) bed_spawn: Option<(u32, u32)>,
    /// Live §4.1 melee arc, if mid-swing.
    pub(crate) swing: Option<super::combat::MeleeSwing>,
    tx: mpsc::Sender<Frame>,
}

impl Player {
    pub(crate) fn center(&self) -> (f32, f32) {
        (
            self.pos.0 + PLAYER_WIDTH / 2.0,
            self.pos.1 + PLAYER_HEIGHT / 2.0,
        )
    }

    pub(crate) fn held_item(&self) -> Option<ItemId> {
        self.inventory
            .get(self.held_slot as usize)
            .copied()
            .flatten()
            .map(|s| s.item)
    }

    /// Royal Gel Charm equipped (§4.3): green/blue slimes never aggro.
    pub(crate) fn slime_friend(&self) -> bool {
        ferraria_shared::loadout::effect_mods(&self.inventory).slime_friend
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
    hp: u16,
    max_hp: u16,
    bed_spawn: Option<(u32, u32)>,
}

/// The authoritative game state, owned by [`run`]'s task. Fields are
/// `pub(crate)` for the sibling sim modules (`interact`, `entities`,
/// `world_tick`), which extend `Sim` with further `impl` blocks.
pub struct Sim {
    pub(crate) world: World,
    pub(crate) tick: u64,
    pub(crate) players: HashMap<u32, Player>,
    offline: HashMap<String, OfflinePlayer>,
    /// Chest origin → player id currently holding it open (§11: one opener
    /// at a time; the reverse link is [`Player::open_chest`]).
    pub(super) chest_locks: HashMap<(u32, u32), u32>,
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
    /// Live fluid automaton over `world` (§3 cadence in `world_tick`).
    pub(crate) fluids: FluidSim,
    /// Server-simulated entities (item drops now; enemies/projectiles later).
    pub(crate) entities: EntityStore,
    /// Accumulated §2 mining damage per cell. Key: `(x, y, wall_layer)`.
    pub(crate) tile_damage: HashMap<(u32, u32, bool), TileDamage>,
    /// Sand cells queued for the §2 falling check next tick.
    pub(crate) sand_active: HashSet<(u32, u32)>,
    /// Player-planted saplings → tick they may grow into a tree (§2 tile 31).
    pub(crate) saplings: HashMap<(u32, u32), u64>,
    /// Level-1 puddle cells → first-seen tick (§3 evaporation).
    pub(crate) puddles: HashMap<(u32, u32), u64>,
    /// Loot/world-event randomness (pot rolls, acorn drops, spawn impulses,
    /// combat crit/proc rolls).
    pub(crate) loot_rng: Pcg32,
    /// §5.3 spawn rolls and AI randomness.
    pub(crate) spawn_rng: Pcg32,
    /// Per-source enemy hit immunity (§0): `(entity, source)` → immune
    /// until tick. [`super::combat::DamageSource`] keeps player-melee and
    /// projectile windows in separate keyspaces (the raw ids overlap).
    pub(crate) enemy_iframes: HashMap<(u32, super::combat::DamageSource), u64>,
    /// Cells changed by batched systems this tick, flushed as one
    /// [`ServerMessage::TilesChanged`] per subscribed player.
    tile_batch: Vec<(u32, u32)>,
    /// Cells whose fixtures must re-validate their §2 support rules after a
    /// nearby change (torch attachment, furniture floors, door frames).
    /// Queued by [`Sim::queue_support_checks`], drained by
    /// `Sim::revalidate_supports` after every command and every tick.
    pub(crate) support_checks: Vec<(u32, u32)>,
    /// Town NPC roster + arrival bookkeeping (§7, `sim::npc`).
    pub(crate) town: TownState,
}

impl Sim {
    pub fn new(world: World, player_count: Arc<AtomicUsize>) -> Sim {
        let fluids = FluidSim::new(&world);
        let mut sim = Sim {
            world,
            tick: 0,
            players: HashMap::new(),
            offline: HashMap::new(),
            chest_locks: HashMap::new(),
            next_player_id: 1,
            next_epoch: 1,
            chunk_cache: HashMap::new(),
            token_rng: Pcg32::new(entropy_seed()),
            pending_kicks: Vec::new(),
            player_count,
            fluids,
            entities: EntityStore::new(),
            tile_damage: HashMap::new(),
            sand_active: HashSet::new(),
            saplings: HashMap::new(),
            puddles: HashMap::new(),
            loot_rng: Pcg32::new(entropy_seed() ^ 0x1007_caf3),
            spawn_rng: Pcg32::new(entropy_seed() ^ 0x57a4_11fe),
            enemy_iframes: HashMap::new(),
            tile_batch: Vec::new(),
            support_checks: Vec::new(),
            town: TownState::default(),
        };
        sim.scan_initial_puddles();
        // §7.2: the Sage is here from the start. NPC state isn't persisted
        // yet, so every boot counts as "first world boot".
        sim.init_npcs();
        sim
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
        // Fixtures whose support the command removed pop before anything
        // else observes the world.
        self.revalidate_supports();
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

        // Live world systems (fluids §3, sand/grass/saplings §2), entities,
        // and town NPCs (§7: dawn events, hourly arrival checks, AI). Their
        // batched cell changes flush as one `TilesChanged` per player at
        // the end of the tick.
        self.world_tick();
        self.tick_entities();
        // Enemies & combat (§5) and player survival (§8).
        self.tick_enemy_spawning();
        self.tick_enemies();
        self.tick_projectiles();
        self.tick_swings();
        self.tick_enemy_contact();
        self.tick_player_vitals();
        self.purge_enemy_iframes();
        // Town NPCs (§7).
        self.tick_npcs();
        self.revalidate_supports();

        // Chest locks follow their openers: walking out of reach closes the
        // chest (§11 — the client mirrors the same rule).
        self.close_out_of_reach_chests();

        self.flush_held_item_broadcasts();
        self.flush_tile_batch();
        if self.tick.is_multiple_of(SNAPSHOT_INTERVAL_TICKS as u64) {
            self.broadcast_entity_updates();
        }

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
        let rec = match reclaimed {
            Some(rec) => rec,
            None => {
                let id = self.next_player_id;
                self.next_player_id += 1;
                OfflinePlayer {
                    id,
                    token: self.fresh_token(),
                    pos: spawn_pos(&self.world),
                    held_slot: 0,
                    inventory: starting_inventory(),
                    hp: PLAYER_BASE_MAX_HP as u16,
                    max_hp: PLAYER_BASE_MAX_HP as u16,
                    bed_spawn: None,
                }
            }
        };
        let OfflinePlayer {
            id,
            token,
            pos,
            held_slot,
            inventory: inv,
            hp,
            max_hp,
            bed_spawn,
        } = rec;
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
            open_chest: None,
            chunks: HashSet::new(),
            last_state_tick: self.tick,
            // One tick of allowance until the first state replenishes it.
            move_budget: MAX_TELEPORT_PER_TICK,
            last_swing_tick: None,
            last_door_toggle_tick: None,
            last_held_broadcast_tick: None,
            held_broadcast_dirty: false,
            hp,
            max_hp,
            dead: false,
            respawn_ready_tick: 0,
            iframe_until: 0,
            last_damage_tick: 0,
            regen_acc: 0.0,
            burn_acc: 0.0,
            drown_acc: 0.0,
            breath: ferraria_shared::PLAYER_MAX_BREATH as u16,
            // Debuffs don't survive a disconnect (they'd have ticked away
            // offline anyway).
            debuffs: Vec::new(),
            fall_accum: 0.0,
            was_grounded: true,
            bed_spawn,
            swing: None,
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
        self.broadcast(&ServerMessage::PlayerHealth { id, hp, max_hp });

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
            encode(&ServerMessage::PlayerHealth { id, hp, max_hp }).into(),
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
            frames.push(
                encode(&ServerMessage::PlayerHealth {
                    id: oid,
                    hp: other.hp,
                    max_hp: other.max_hp,
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
        // Town roster + own health, after the chunk stream (clients don't
        // care about ordering; it keeps the join-state frame sequence that
        // pre-NPC tests pin intact).
        let roster = self.npc_list_message();
        self.send_to(id, &roster);
        self.send_to(id, &ServerMessage::PlayerHealth { id, hp, max_hp });
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
        self.release_chest(id); // free any chest lock (§11)
        let Some(p) = self.players.remove(&id) else {
            return; // disconnect raced a kick — already gone
        };
        tracing::info!(player = id, name = %p.name, "player left");
        // A player who disconnects while dead reconnects respawned (bed or
        // world spawn, §8 respawn HP).
        let (pos, hp) = if p.dead {
            (
                p.bed_spawn
                    .and_then(|o| super::survival::bed_spawn_pos(&self.world, o))
                    .unwrap_or_else(|| super::survival::spawn_pos(&self.world)),
                (ferraria_shared::PLAYER_BASE_MAX_HP.max(p.max_hp as u32 / 2) as u16).min(p.max_hp),
            )
        } else {
            (p.pos, p.hp)
        };
        self.offline.insert(
            p.name,
            OfflinePlayer {
                id,
                token: p.token,
                pos,
                held_slot: p.held_slot,
                inventory: p.inventory,
                hp,
                max_hp: p.max_hp,
                bed_spawn: p.bed_spawn,
            },
        );
        self.player_count
            .store(self.players.len(), Ordering::Relaxed);
        self.broadcast(&ServerMessage::PlayerLeft { id });
    }

    /// §7.2 "All players' combined inventory coins": online sessions plus
    /// the parked offline records. Nothing is counted twice — a reclaim
    /// removes the offline record before the session goes online, and
    /// `join` rejects a name that is already online.
    pub(crate) fn combined_player_coins(&self) -> u64 {
        let online: u64 = self
            .players
            .values()
            .map(|p| coin_total(&p.inventory))
            .sum();
        let offline: u64 = self
            .offline
            .values()
            .map(|rec| coin_total(&rec.inventory))
            .sum();
        online + offline
    }

    /// §7.2 "Any player has max HP > `hp`", online or parked offline (same
    /// no-double-count argument as [`Sim::combined_player_coins`]).
    pub(crate) fn any_player_max_hp_over(&self, hp: u32) -> bool {
        self.players.values().any(|p| u32::from(p.max_hp) > hp)
            || self.offline.values().any(|rec| u32::from(rec.max_hp) > hp)
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
        // Dead players (§8) can chat, ping, reorganize inventory, and ask to
        // respawn — but can't act on the world.
        if self.players.get(&id).is_some_and(|p| p.dead)
            && !matches!(
                msg,
                ClientMessage::Ping { .. }
                    | ClientMessage::Chat { .. }
                    | ClientMessage::Respawn
                    | ClientMessage::SelectSlot { .. }
                    | ClientMessage::MoveSlot { .. }
                    | ClientMessage::SplitSlot { .. }
            )
        {
            return;
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
            ClientMessage::HitTile { x, y } => self.hit_tile(id, x, y),
            ClientMessage::HitWall { x, y } => self.hit_wall(id, x, y),
            ClientMessage::PlaceTile { x, y, hotbar_slot } => {
                self.place_tile(id, x, y, hotbar_slot)
            }
            ClientMessage::PlaceWall { x, y, hotbar_slot } => {
                self.place_wall(id, x, y, hotbar_slot)
            }
            ClientMessage::ToggleDoor { x, y } => self.toggle_door(id, x, y),
            ClientMessage::MoveSlot { from, to } => self.inv_slot_op(id, from, to, SlotOp::Move),
            ClientMessage::SplitSlot { from, to } => {
                self.inv_slot_op(id, from, to, SlotOp::SplitHalf)
            }
            ClientMessage::DropItem { slot, count } => self.drop_item(id, slot, count),
            ClientMessage::Craft { recipe_id } => self.craft(id, recipe_id),
            ClientMessage::OpenChest { x, y } => self.open_chest(id, x, y),
            ClientMessage::CloseChest => self.close_chest(id),
            ClientMessage::ChestMoveSlot {
                chest_slot,
                inv_slot,
                to_chest,
            } => self.chest_move_slot(id, chest_slot, inv_slot, to_chest),
            ClientMessage::UseItem { slot, aim } => self.use_item(id, slot, aim),
            ClientMessage::Respawn => self.respawn_player(id),
            ClientMessage::TalkNpc { npc_id } => self.talk_npc(id, npc_id),
            ClientMessage::BuyItem {
                npc_id,
                item,
                count,
            } => self.buy_item(id, npc_id, item, count),
            ClientMessage::SellItem {
                npc_id,
                slot,
                count,
            } => self.sell_item(id, npc_id, slot, count),
            ClientMessage::NurseHeal => self.nurse_heal(id),
            ClientMessage::SetBedSpawn { x, y } => self.set_bed_spawn(id, x, y),
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
        let old_pos = p.pos;
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
        // Fall-damage observation (§8, sim::survival).
        self.observe_movement(id, old_pos, anim);
    }

    fn select_slot(&mut self, id: u32, slot: u8) {
        if slot as usize >= inventory::HOTBAR {
            return;
        }
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        p.held_slot = slot;
        // Anti-amplification: a SelectSlot flood coalesces to one broadcast
        // per HELD_ITEM_BROADCAST_MIN_TICKS; the trailing selection is
        // flushed by `flush_held_item_broadcasts` when the window elapses.
        if p.last_held_broadcast_tick
            .is_none_or(|t| tick.saturating_sub(t) >= HELD_ITEM_BROADCAST_MIN_TICKS)
        {
            p.last_held_broadcast_tick = Some(tick);
            p.held_broadcast_dirty = false;
            self.broadcast_held_item(id);
        } else {
            p.held_broadcast_dirty = true;
        }
    }

    /// Sends the deferred (coalesced) selection broadcasts whose window
    /// elapsed this tick — so a slot-spamming client ends up announcing its
    /// final selection, just rate-capped.
    fn flush_held_item_broadcasts(&mut self) {
        let tick = self.tick;
        let due: Vec<u32> = self
            .players
            .iter()
            .filter(|(_, p)| {
                p.held_broadcast_dirty
                    && p.last_held_broadcast_tick
                        .is_none_or(|t| tick.saturating_sub(t) >= HELD_ITEM_BROADCAST_MIN_TICKS)
            })
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            if let Some(p) = self.players.get_mut(&id) {
                p.held_broadcast_dirty = false;
                p.last_held_broadcast_tick = Some(tick);
            }
            self.broadcast_held_item(id);
        }
    }

    /// Re-announces what `id` is holding (slot selection, or the held stack
    /// changed/emptied through placement or pickup). Remote clients render
    /// it; the owner already knows their own inventory.
    pub(crate) fn broadcast_held_item(&mut self, id: u32) {
        let Some(p) = self.players.get(&id) else {
            return;
        };
        let frame: Frame = encode(&ServerMessage::PlayerHeldItem {
            id,
            slot: p.held_slot,
            item: p.held_item(),
        })
        .into();
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

    /// The single *immediate* tile-mutation point: writes the tile,
    /// invalidates the chunk cache, pushes the delta to every subscribed
    /// player, and wakes the world systems (fluids, sand, puddles, mining
    /// damage) around the cell. Player-driven changes (mining, placing,
    /// doors) go through here; high-volume systems use [`Sim::stage_tile`] +
    /// the per-tick `TilesChanged` flush instead.
    pub fn change_tile(&mut self, x: u32, y: u32, tile: Tile) {
        if !self.world.in_bounds(x, y) {
            return;
        }
        self.world.set_tile(x, y, tile);
        self.chunk_cache
            .remove(&((x / CHUNK_SIZE), (y / CHUNK_SIZE)));
        self.broadcast_at(x, y, &ServerMessage::TileChanged { x, y, tile });
        self.wake_cell(x, y);
    }

    /// Batched counterpart of [`Sim::change_tile`]: the world cell must
    /// already be written; this invalidates the chunk cache and queues the
    /// cell for the end-of-tick [`ServerMessage::TilesChanged`] flush.
    pub(crate) fn stage_tile(&mut self, x: u32, y: u32) {
        self.chunk_cache
            .remove(&((x / CHUNK_SIZE), (y / CHUNK_SIZE)));
        self.tile_batch.push((x, y));
    }

    /// Wakes the systems watching a changed cell: re-marks the fluid
    /// neighborhood, queues sand-fall checks (this cell and the one above),
    /// queues §2 support re-validation for the neighborhood's fixtures,
    /// refreshes puddle tracking, and clears stale mining damage.
    pub(crate) fn wake_cell(&mut self, x: u32, y: u32) {
        self.tile_damage.remove(&(x, y, false));
        self.tile_damage.remove(&(x, y, true));
        self.fluids.mark(x, y);
        self.fluids.mark(x.wrapping_sub(1), y);
        self.fluids.mark(x + 1, y);
        self.fluids.mark(x, y.wrapping_sub(1));
        self.fluids.mark(x, y + 1);
        for (sx, sy) in [(x, y), (x, y.wrapping_sub(1))] {
            if self.world.in_bounds(sx, sy)
                && self.world.tile(sx, sy).id == ferraria_shared::tiles::TileId::Sand
            {
                self.sand_active.insert((sx, sy));
            }
        }
        self.queue_support_checks(x, y);
        self.track_puddle(x, y);
        self.track_puddle(x, y.wrapping_sub(1));
        self.track_puddle(x, y + 1);
    }

    /// Queues the changed cell and its cardinal neighbors for §2 fixture
    /// support re-validation (torch attachment, furniture floors, door
    /// frames — see `Sim::revalidate_supports`).
    pub(crate) fn queue_support_checks(&mut self, x: u32, y: u32) {
        for (cx, cy) in [
            (x, y),
            (x.wrapping_sub(1), y),
            (x + 1, y),
            (x, y.wrapping_sub(1)),
            (x, y + 1),
        ] {
            if self.world.in_bounds(cx, cy) {
                self.support_checks.push((cx, cy));
            }
        }
    }

    /// Sends the accumulated batched cell changes as one `TilesChanged` per
    /// player, filtered to each player's subscribed chunks.
    fn flush_tile_batch(&mut self) {
        if self.tile_batch.is_empty() {
            return;
        }
        let mut cells = std::mem::take(&mut self.tile_batch);
        cells.sort_unstable();
        cells.dedup();
        let changes: Vec<(u32, u32, Tile)> = cells
            .into_iter()
            .filter(|&(x, y)| self.world.in_bounds(x, y))
            .map(|(x, y)| (x, y, self.world.tile(x, y)))
            .collect();
        let ids: Vec<u32> = self.players.keys().copied().collect();
        for id in ids {
            let Some(p) = self.players.get(&id) else {
                continue;
            };
            let mine: Vec<(u32, u32, Tile)> = changes
                .iter()
                .filter(|&&(x, y, _)| p.chunks.contains(&(x / CHUNK_SIZE, y / CHUNK_SIZE)))
                .copied()
                .collect();
            if !mine.is_empty() {
                self.send_to(id, &ServerMessage::TilesChanged { changes: mine });
            }
        }
    }

    /// Broadcasts a message to every player subscribed to the chunk
    /// containing cell `(x, y)`.
    pub(crate) fn broadcast_at(&mut self, x: u32, y: u32, msg: &ServerMessage) {
        let chunk = (x / CHUNK_SIZE, y / CHUNK_SIZE);
        let frame: Frame = encode(msg).into();
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
    pub(crate) fn update_player_chunks(&mut self, id: u32) {
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
        // Entities already living in newly entered chunks stream alongside
        // the terrain (their spawn broadcasts only reached players whose
        // window contained them at the time).
        let entity_frames: Vec<Frame> = frames
            .iter()
            .flat_map(|&(c, _)| self.entities.spawn_messages_in_chunk(c))
            .map(|msg| -> Frame { encode(&msg).into() })
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
            for frame in entity_frames {
                if p.tx.try_send(frame).is_err() {
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

    pub(crate) fn send_to(&mut self, id: u32, msg: &ServerMessage) {
        self.send_frame_to(id, encode(msg).into());
    }

    fn send_frame_to(&mut self, id: u32, frame: Frame) {
        if let Some(p) = self.players.get(&id) {
            if p.tx.try_send(frame).is_err() {
                self.pending_kicks.push(id);
            }
        }
    }

    pub(crate) fn broadcast(&mut self, msg: &ServerMessage) {
        let frame: Frame = encode(msg).into();
        self.broadcast_frame(&frame, None);
    }

    pub(super) fn broadcast_frame(&mut self, frame: &Frame, except: Option<u32>) {
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
pub(crate) fn spawn_pos(world: &World) -> (f32, f32) {
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

/// Test plumbing shared by this module's tests and `sim::inventory`'s.
#[cfg(test)]
pub(super) mod testutil {
    use super::*;
    use ferraria_shared::protocol::decode;

    /// A sim over a small empty world with an inspectable player counter.
    pub fn test_sim() -> Sim {
        let mut world = World::new(320, 320);
        world.spawn = (160, 100);
        Sim::new(world, Arc::new(AtomicUsize::new(0)))
    }

    /// Joins a player, returning their id, session epoch, and the outbound
    /// frame receiver.
    pub fn join(
        sim: &mut Sim,
        name: &str,
        token: Option<AuthToken>,
    ) -> (u32, u64, mpsc::Receiver<Frame>) {
        let (reply, rx) = try_join(sim, name, token);
        let (id, epoch) = reply.expect("join accepted");
        (id, epoch, rx)
    }

    pub fn try_join(
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
    pub fn msg(sim: &mut Sim, player_id: u32, epoch: u64, msg: ClientMessage) {
        sim.handle(SimCommand::Message {
            player_id,
            epoch,
            msg,
        });
    }

    pub fn drain(rx: &mut mpsc::Receiver<Frame>) -> Vec<ServerMessage> {
        let mut out = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            out.push(decode::<ServerMessage>(&frame).expect("valid frame"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use ferraria_shared::world::NEW_WORLD_TIME;

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
    fn select_slot_floods_coalesce_to_rate_capped_broadcasts() {
        let mut sim = test_sim();
        let (b, b_epoch, _rx_b) = join(&mut sim, "bob", None);
        let (_a, _, mut rx_a) = join(&mut sim, "alice", None);
        drain(&mut rx_a);
        // 8 selections within one tick: a hostile client could otherwise
        // amplify each into a PlayerHeldItem broadcast at socket speed.
        for slot in [1u8, 2, 3, 4, 5, 4, 3, 2] {
            msg(&mut sim, b, b_epoch, ClientMessage::SelectSlot { slot });
        }
        let held: Vec<u8> = drain(&mut rx_a)
            .into_iter()
            .filter_map(|m| match m {
                ServerMessage::PlayerHeldItem { id, slot, .. } if id == b => Some(slot),
                _ => None,
            })
            .collect();
        assert_eq!(held, vec![1], "only the first broadcast immediately");
        assert_eq!(
            sim.players[&b].held_slot, 2,
            "server state still tracks the final selection"
        );
        // The trailing selection flushes once the window elapses — remote
        // clients converge on what the spammer ends up holding.
        for _ in 0..HELD_ITEM_BROADCAST_MIN_TICKS {
            sim.tick();
        }
        let held: Vec<u8> = drain(&mut rx_a)
            .into_iter()
            .filter_map(|m| match m {
                ServerMessage::PlayerHeldItem { id, slot, .. } if id == b => Some(slot),
                _ => None,
            })
            .collect();
        assert_eq!(held, vec![2], "trailing selection flushed exactly once");
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
