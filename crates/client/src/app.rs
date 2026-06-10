//! The client app state machine: Menu → Connecting → Playing →
//! Disconnected, and the in-game `Session` (world mirror, prediction,
//! interpolation, rendering, UI) that runs while Playing.

use std::collections::HashMap;

use macroquad::prelude::*;

use ferraria_shared::items::{inventory, ArmorSlot, InvSlot, ItemId};
use ferraria_shared::physics::{PlayerInput, PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::{AuthToken, ClientMessage, ServerMessage};
use ferraria_shared::tiles::{MINING_HELMET_LIGHT, PLAYER_GLOW};
use ferraria_shared::world::{WorldFlags, DAY_TICKS};
use ferraria_shared::{MAX_NAME_CHARS, PROTOCOL_VERSION, TICK_RATE, TILE_SIZE};

use crate::entities::Entities;
use crate::hotbar;
use crate::interact::Interact;
use crate::light::{self, DynamicSource, LightEngine};
use crate::net::{WsClient, WsStatus};
use crate::player::{OwnPlayer, RemotePlayer, Snapshot, CORRECTION_SNAP_TILES, INTERP_DELAY};
use crate::render::{self, Camera, PlayerDraw, QuadBatch};
use crate::ui::{self, Chat, DisconnectedChoice, Hud};
use crate::world_view::WorldView;

/// Give the socket + handshake this long before declaring failure.
const CONNECT_TIMEOUT_SECS: f64 = 15.0;

pub struct App {
    state: State,
    /// Persist across reconnects within the page session. (The auth token in
    /// browser localStorage comes with the persistence PR.)
    name: String,
    token: Option<AuthToken>,
}

enum State {
    Menu {
        error: Option<String>,
    },
    Connecting {
        /// `Option` only so the client can be moved into the `Session`.
        ws: Option<WsClient>,
        hello_sent: bool,
        started: f64,
    },
    Playing(Box<Session>),
    Disconnected {
        reason: String,
    },
}

impl App {
    pub fn new() -> App {
        App {
            state: State::Menu { error: None },
            name: String::new(),
            token: None,
        }
    }

    /// Runs one render frame; called from the macroquad loop.
    pub fn frame(&mut self) {
        let next = match &mut self.state {
            State::Menu { error } => {
                ui::text_input(&mut self.name, MAX_NAME_CHARS);
                let join = ui::draw_menu(&self.name, error.as_deref());
                if (join || is_key_pressed(KeyCode::Enter)) && !self.name.trim().is_empty() {
                    Some(connect_state())
                } else {
                    None
                }
            }
            State::Connecting {
                ws,
                hello_sent,
                started,
            } => {
                // No text field is live here: drop this frame's typed chars
                // so they can't leak into the name field back in Menu.
                ui::discard_typed_chars();
                ui::draw_connecting(get_time() - *started);
                connecting_frame(ws, hello_sent, *started, &self.name, self.token)
            }
            State::Playing(session) => {
                let result = session.frame();
                self.token = Some(session.token);
                result.map(|reason| State::Disconnected { reason })
            }
            State::Disconnected { reason } => {
                // Same: e.g. a chat message in flight when the socket died
                // must not be typed into the name field later.
                ui::discard_typed_chars();
                match ui::draw_disconnected(reason) {
                    DisconnectedChoice::Reconnect => Some(connect_state()),
                    DisconnectedChoice::Menu => Some(State::Menu { error: None }),
                    DisconnectedChoice::None => None,
                }
            }
        };
        if let Some(next) = next {
            self.state = next;
        }
    }
}

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

fn connect_state() -> State {
    match WsClient::connect_to_page_server() {
        Ok(ws) => State::Connecting {
            ws: Some(ws),
            hello_sent: false,
            started: get_time(),
        },
        Err(reason) => State::Disconnected { reason },
    }
}

/// Drives the socket-open + Hello/Welcome handshake. Any chunk/roster frames
/// that arrive in the same drain as `Welcome` are fed into the new session
/// so nothing is dropped.
fn connecting_frame(
    ws: &mut Option<WsClient>,
    hello_sent: &mut bool,
    started: f64,
    name: &str,
    token: Option<AuthToken>,
) -> Option<State> {
    let client = ws.as_mut()?;
    let status = client.status();
    if status == WsStatus::Open && !*hello_sent {
        *hello_sent = true;
        client.send(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            name: name.trim().to_string(),
            token,
        });
    }
    if status != WsStatus::Connecting {
        // Drain even when the socket already closed: a rejecting server
        // sends `Reject` and immediately closes, and that reason must win
        // over the generic message below.
        let msgs = client.drain();
        let mut iter = msgs.into_iter();
        for msg in iter.by_ref() {
            match msg {
                ServerMessage::Welcome {
                    player_id,
                    token,
                    world_width,
                    world_height,
                    spawn,
                    time,
                    day,
                    flags,
                } => {
                    let ws = ws.take()?;
                    let mut session = Session::new(
                        ws,
                        name.trim().to_string(),
                        Welcome {
                            player_id,
                            token,
                            world_width,
                            world_height,
                            spawn,
                            time,
                            day,
                            flags,
                        },
                    );
                    let now = get_time();
                    for rest in iter {
                        session.apply(rest, now);
                    }
                    return Some(State::Playing(Box::new(session)));
                }
                ServerMessage::Reject { reason } => return Some(State::Disconnected { reason }),
                _ => {} // nothing else is valid pre-Welcome; ignore
            }
        }
    }
    if status == WsStatus::Closed {
        return Some(State::Disconnected {
            reason: "could not reach the server".into(),
        });
    }
    if get_time() - started > CONNECT_TIMEOUT_SECS {
        return Some(State::Disconnected {
            reason: "timed out waiting for the server".into(),
        });
    }
    None
}

/// The `Welcome` payload, destructured.
struct Welcome {
    player_id: u32,
    token: AuthToken,
    world_width: u32,
    world_height: u32,
    spawn: (u32, u32),
    time: u32,
    day: u32,
    flags: WorldFlags,
}

/// Day clock, advanced client-side at 60 ticks/s between `TimeSync`s (§9).
struct GameClock {
    /// Tick-of-day with fractional accumulation.
    time: f64,
    day: u32,
}

impl GameClock {
    fn advance(&mut self, dt: f32) {
        self.time += dt as f64 * TICK_RATE as f64;
        while self.time >= DAY_TICKS as f64 {
            self.time -= DAY_TICKS as f64;
            self.day += 1;
        }
    }

    fn set(&mut self, time: u32, day: u32) {
        self.time = time as f64;
        self.day = day;
    }

    fn ticks(&self) -> u32 {
        self.time as u32
    }
}

/// Everything live while connected and in-world.
struct Session {
    ws: WsClient,
    view: WorldView,
    own_id: u32,
    token: AuthToken,
    name: String,
    own: OwnPlayer,
    remotes: HashMap<u32, RemotePlayer>,
    clock: GameClock,
    camera: Camera,
    chat: Chat,
    hud: Hud,
    /// Server-authoritative inventory mirror (flat §8 layout). Also feeds
    /// lighting: a Mining Helmet in the head armor slot is a light source.
    inventory: Vec<Option<InvSlot>>,
    /// Selected hotbar slot (0–9); the server learns via `SelectSlot`.
    selected: u8,
    interact: Interact,
    entities: Entities,
    light: LightEngine,
    /// Reusable buffers: per-vertex-colored quads (darkness overlay, sky,
    /// vignette), this frame's dynamic light sources, the remote-player
    /// snapshots sampled once per frame (lighting + drawing share them), and
    /// the visible rect's corner-brightness grid for `draw_world`.
    batch: QuadBatch,
    sources: Vec<DynamicSource>,
    remote_samples: Vec<(u32, Snapshot)>,
    corners: Vec<f32>,
    /// F3 stats: duration of the last light recompute and how many ran.
    light_ms: f64,
    light_recomputes: u64,
    /// World progress flags, mirrored for later UI (bosses defeated...).
    #[allow(dead_code)]
    flags: WorldFlags,
}

impl Session {
    fn new(ws: WsClient, name: String, welcome: Welcome) -> Session {
        let view = WorldView::new(welcome.world_width, welcome.world_height, welcome.spawn);
        let own = OwnPlayer::at_spawn(welcome.spawn);
        let center = own.phys.center();
        let mut camera = Camera::new(Vec2::ZERO);
        camera.snap(vec2(center.0, center.1) * TILE_SIZE, world_px(&view));
        Session {
            ws,
            light: LightEngine::new(welcome.world_width, welcome.world_height),
            view,
            own_id: welcome.player_id,
            token: welcome.token,
            name,
            own,
            remotes: HashMap::new(),
            clock: GameClock {
                time: welcome.time as f64,
                day: welcome.day,
            },
            camera,
            chat: Chat::new(),
            hud: Hud::new(),
            inventory: vec![None; inventory::TOTAL],
            selected: 0,
            interact: Interact::new(),
            entities: Entities::new(),
            batch: QuadBatch::new(),
            sources: Vec::new(),
            remote_samples: Vec::new(),
            corners: Vec::new(),
            light_ms: 0.0,
            light_recomputes: 0,
            flags: welcome.flags,
        }
    }

    /// One frame while Playing. Returns a reason to move to Disconnected.
    fn frame(&mut self) -> Option<String> {
        let now = get_time();
        let dt = get_frame_time();

        // 1. Network in.
        for msg in self.ws.drain() {
            self.apply(msg, now);
        }
        if self.ws.is_closed() {
            return Some("connection to the server was lost".into());
        }

        // 2. Input. Chat captures the keyboard while open.
        if is_key_pressed(KeyCode::F3) {
            self.hud.debug = !self.hud.debug;
        }
        if let Some(text) = self.chat.handle_input() {
            self.ws.send(&ClientMessage::Chat { text });
        }
        let input = if self.chat.open {
            PlayerInput::default()
        } else {
            gather_input()
        };

        // 3. Own-player prediction at a fixed 60 Hz, frozen until the chunk
        // under us has streamed in.
        let center = self.own.phys.center();
        let frozen = !self.view.chunk_loaded_at(center.0, center.1);
        for msg in self.own.update(self.view.world(), input, dt, frozen) {
            self.ws.send(&msg);
        }

        // 4. Clock & camera.
        self.clock.advance(dt);
        let center = self.own.phys.center();
        self.camera.follow(
            vec2(center.0, center.1) * TILE_SIZE,
            world_px(&self.view),
            dt,
        );

        // 4.5. Mouse world interaction (mining/placing/doors) and hotbar
        // selection, with the fresh camera. Chat owns the keyboard while
        // open, so hotbar keys stay quiet then.
        let tl = self.camera.top_left();
        let aim = Interact::aim(self.view.world(), tl);
        if !self.chat.open {
            if hotbar::selection_input(&mut self.selected) {
                self.ws.send(&ClientMessage::SelectSlot {
                    slot: self.selected,
                });
            }
            self.interact.frame(
                &self.ws,
                self.view.world(),
                center,
                &self.inventory,
                self.selected,
                aim,
                dt,
            );
        }

        // 5. Lighting. Sample remote players once (lighting and drawing
        // share the snapshots), gather this frame's dynamic sources, and let
        // the engine decide whether the visible region needs a recompute.
        let render_t = now - INTERP_DELAY;
        self.remote_samples.clear();
        for (&id, remote) in self.remotes.iter_mut() {
            self.remote_samples.push((id, remote.sample(render_t)));
        }
        // Stable order: dynamic sources are compared against last frame's to
        // skip recomputes, which map iteration order must not defeat.
        self.remote_samples.sort_by_key(|&(id, _)| id);
        let ticks = self.clock.ticks();
        self.gather_light_sources();
        let view_rect = self.camera.visible_tiles(self.view.world());
        let abs_tick = self.clock.day as u64 * DAY_TICKS as u64 + ticks as u64;
        let t0 = get_time();
        if self
            .light
            .update(self.view.world(), view_rect, ticks, abs_tick, &self.sources)
        {
            self.light_ms = (get_time() - t0) * 1000.0;
            self.light_recomputes += 1;
        }

        // 6. Draw: sky backdrop, lit world, cracks, item drops, players,
        // night vignette. Cracks and drops sample the light field so they
        // fade into darkness with the tiles around them.
        clear_background(render::sky_color(ticks));
        render::draw_sky(ticks, now, &mut self.batch);
        render::draw_world(
            &self.view,
            &self.camera,
            &self.light,
            &mut self.batch,
            &mut self.corners,
        );
        self.interact.draw_cracks(now, tl, &self.light);
        self.entities.draw(render_t, now, tl, &self.light);
        for &(id, s) in &self.remote_samples {
            let Some(remote) = self.remotes.get(&id) else {
                continue;
            };
            render::draw_player(&PlayerDraw {
                pos: vec2(s.pos.0 * TILE_SIZE - tl.x, s.pos.1 * TILE_SIZE - tl.y),
                world_x: s.pos.0,
                vel_x: s.vel.0,
                facing: s.facing,
                anim: s.anim,
                name: &remote.name,
                is_self: false,
                held_item: remote.held_item,
                light: self
                    .light
                    .brightness_at(s.pos.0 + PLAYER_WIDTH / 2.0, s.pos.1 + PLAYER_HEIGHT / 2.0),
            });
        }
        let p = &self.own.phys;
        let own_center = p.center();
        render::draw_player(&PlayerDraw {
            pos: vec2(p.pos.0 * TILE_SIZE - tl.x, p.pos.1 * TILE_SIZE - tl.y),
            world_x: p.pos.0,
            vel_x: p.vel.0,
            facing: self.own.facing,
            anim: self.own.anim_flags(),
            name: &self.name,
            is_self: true,
            held_item: self
                .inventory
                .get(self.selected as usize)
                .copied()
                .flatten()
                .map(|s| s.item),
            light: self.light.brightness_at(own_center.0, own_center.1),
        });
        self.interact.draw_aim(aim, center, tl);
        // Night vignette, surface only (skip when buried — caves are dark
        // everywhere, the edges shouldn't be extra-dark).
        let own_tile = (own_center.0 as u32, own_center.1 as u32);
        if self.light.sky_exposed(own_tile.0, own_tile.1) {
            render::draw_vignette(1.0 - light::daylight(ticks), &mut self.batch);
        }

        // 7. Overlay UI.
        self.hud
            .draw(self.remotes.len() + 1, self.clock.day, self.clock.ticks());
        hotbar::draw(&self.inventory, self.selected);
        self.chat.draw(now);
        if self.hud.debug {
            self.hud.draw_debug(
                now,
                self.own.phys.pos,
                self.own.phys.vel,
                self.view.loaded_chunks(),
                self.ws.bad_frames,
                self.light_ms,
                self.light_recomputes,
            );
        }
        None
    }

    /// Collects this frame's non-tile light sources (§10): every player
    /// glows 4 at their center tile; a worn Mining Helmet emits 20 at the
    /// own player's head. Remote helmets aren't knowable yet — the server
    /// doesn't broadcast worn armor (only held items); if that lands, hook
    /// it in here.
    fn gather_light_sources(&mut self) {
        self.sources.clear();
        let center = self.own.phys.center();
        self.sources.push(DynamicSource {
            x: center.0 as i32,
            y: center.1 as i32,
            level: PLAYER_GLOW,
        });
        // Specifically the head slot (§10 ties the light to *wearing* the
        // helmet): inventory slot `ARMOR_START + k`, in `ArmorSlot`
        // declaration order (Head, Chest, Legs).
        let head = self
            .inventory
            .get(inventory::ARMOR_START + ArmorSlot::Head as usize)
            .copied()
            .flatten()
            .map(|s| s.item);
        if head == Some(ItemId::MiningHelmet) {
            let head_y = self.own.phys.pos.1 + render::HEAD_CENTER_PX / TILE_SIZE;
            self.sources.push(DynamicSource {
                x: center.0 as i32,
                y: head_y as i32,
                level: MINING_HELMET_LIGHT,
            });
        }
        for &(_, s) in &self.remote_samples {
            self.sources.push(DynamicSource {
                x: (s.pos.0 + PLAYER_WIDTH / 2.0) as i32,
                y: (s.pos.1 + PLAYER_HEIGHT / 2.0) as i32,
                level: PLAYER_GLOW,
            });
        }
    }

    /// Applies one server message to the session state.
    fn apply(&mut self, msg: ServerMessage, now: f64) {
        match msg {
            ServerMessage::ChunkData { cx, cy, bytes } => {
                match self.view.apply_chunk(cx, cy, &bytes) {
                    Ok(()) => self.light.on_chunk_applied(self.view.world(), cx, cy),
                    Err(e) => warn!("dropping bad chunk ({cx},{cy}): {e}"),
                }
            }
            ServerMessage::TileChanged { x, y, tile } => {
                self.view.apply_tile(x, y, tile);
                self.interact.on_tile_changed(x, y);
                self.light.on_tile_changed(self.view.world(), x, y);
            }
            ServerMessage::TilesChanged { changes } => {
                for (x, y, tile) in changes {
                    self.view.apply_tile(x, y, tile);
                    self.interact.on_tile_changed(x, y);
                    self.light.on_tile_changed(self.view.world(), x, y);
                }
            }
            ServerMessage::BlockCrack { x, y, damage_frac } => {
                self.interact.on_block_crack(x, y, damage_frac, now);
            }
            // The armor slots inside the mirror feed lighting too: a worn
            // Mining Helmet is a light source (§10).
            ServerMessage::InventorySync { slots } => {
                self.inventory = slots;
                self.inventory.resize(inventory::TOTAL, None);
            }
            ServerMessage::SlotChanged { idx, stack } => {
                if let Some(slot) = self.inventory.get_mut(idx as usize) {
                    *slot = stack;
                }
            }
            ServerMessage::ItemDropSpawn {
                id,
                item,
                count,
                pos,
                vel,
            } => self.entities.spawn_item(id, item, count, pos, vel, now),
            ServerMessage::EntitySpawn {
                id, kind, pos, vel, ..
            } => self.entities.spawn_other(id, kind, pos, vel, now),
            ServerMessage::EntityUpdate { entities } => self.entities.update(&entities, now),
            ServerMessage::EntityDespawn { id, .. } => self.entities.remove(id),
            ServerMessage::ItemPickedUp { id, .. } => self.entities.remove(id),
            ServerMessage::PlayerJoined { id, name, pos } => {
                if id != self.own_id {
                    self.chat.push_system(format!("{name} joined"), now);
                    self.remotes.insert(id, RemotePlayer::new(name, pos, now));
                }
            }
            ServerMessage::PlayerLeft { id } => {
                if let Some(p) = self.remotes.remove(&id) {
                    self.chat.push_system(format!("{} left", p.name), now);
                }
            }
            ServerMessage::PlayerMoved {
                id,
                pos,
                vel,
                facing,
                anim,
            } => {
                if id == self.own_id {
                    // The server only echoes our own id to correct us
                    // (teleport rejection, reconnect reclaim) — snap if the
                    // disagreement is real.
                    let (dx, dy) = (pos.0 - self.own.phys.pos.0, pos.1 - self.own.phys.pos.1);
                    if dx * dx + dy * dy > CORRECTION_SNAP_TILES * CORRECTION_SNAP_TILES {
                        self.own.apply_correction(pos, vel);
                        // Recenter the camera with the player: a reclaim can
                        // move us across the map, and smoothing there would
                        // pan over unloaded world.
                        let center = self.own.phys.center();
                        let bounds = world_px(&self.view);
                        self.camera
                            .snap(vec2(center.0, center.1) * TILE_SIZE, bounds);
                    }
                } else if let Some(remote) = self.remotes.get_mut(&id) {
                    remote.push(Snapshot {
                        t: now,
                        pos,
                        vel,
                        facing,
                        anim,
                    });
                }
            }
            ServerMessage::PlayerHeldItem { id, item, .. } => {
                if let Some(remote) = self.remotes.get_mut(&id) {
                    remote.held_item = item;
                }
            }
            ServerMessage::Chat { from, text } => self.chat.push_message(&from, &text, now),
            ServerMessage::Toast { text } => self.chat.push_system(text, now),
            ServerMessage::TimeSync { time, day } => self.clock.set(time, day),
            ServerMessage::Reject { .. }
            | ServerMessage::Welcome { .. }
            | ServerMessage::Pong { .. } => {}
            // Entities, NPCs, health: rendered by later PRs.
            _ => {}
        }
    }
}

fn world_px(view: &WorldView) -> Vec2 {
    vec2(
        view.world().width as f32 * TILE_SIZE,
        view.world().height as f32 * TILE_SIZE,
    )
}

/// Movement keys (DESIGN §8): A/D or arrows, Space jump (hold to rise),
/// S/Down + Space drops through platforms.
fn gather_input() -> PlayerInput {
    PlayerInput {
        left: is_key_down(KeyCode::A) || is_key_down(KeyCode::Left),
        right: is_key_down(KeyCode::D) || is_key_down(KeyCode::Right),
        jump: is_key_down(KeyCode::Space),
        down: is_key_down(KeyCode::S) || is_key_down(KeyCode::Down),
    }
}
