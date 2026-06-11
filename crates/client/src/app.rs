//! The client app state machine: Menu → Connecting → Playing →
//! Disconnected, and the in-game `Session` (world mirror, prediction,
//! interpolation, rendering, UI) that runs while Playing.

use std::collections::HashMap;

use macroquad::prelude::*;

use ferraria_shared::crafting::stations_in_range;
use ferraria_shared::items::{inventory, ArmorSlot, InvSlot, ItemId};
use ferraria_shared::loadout;
use ferraria_shared::physics::{PhysicsMods, PlayerInput, PLAYER_HEIGHT, PLAYER_WIDTH};
use ferraria_shared::protocol::{ActiveDebuff, AuthToken, ClientMessage, Debuff, ServerMessage};
use ferraria_shared::tiles::{TileId, MINING_HELMET_LIGHT, PLAYER_GLOW};
use ferraria_shared::world::{chest_in_reach, WorldFlags, DAY_TICKS};
use ferraria_shared::{
    DARKNESS_LIGHT_RADIUS_MULT, MAX_NAME_CHARS, PLAYER_BASE_MAX_HP, PLAYER_MAX_BREATH,
    PROTOCOL_VERSION, RESPAWN_SECS, TICK_RATE, TILE_SIZE,
};

use crate::entities::Entities;
use crate::interact::Interact;
use crate::light::{self, DynamicSource, LightEngine};
use crate::net::{WsClient, WsStatus};
use crate::npcs::Npcs;
use crate::player::{OwnPlayer, RemotePlayer, Snapshot, CORRECTION_SNAP_TILES, INTERP_DELAY};
use crate::render::{self, Camera, PlayerDraw, QuadBatch};
use crate::ui::crafting::CraftingUi;
use crate::ui::dialogue::DialogueUi;
use crate::ui::inventory::{ChestMirror, InventoryUi};
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
    /// Server-authoritative inventory mirror (flat §8 layout), updated by
    /// `InventorySync` / `SlotChanged`. Also feeds lighting (a Mining Helmet
    /// in the head armor slot is a light source).
    inventory: Vec<Option<InvSlot>>,
    /// Equipment physics modifiers derived from `inventory` (recomputed on
    /// every inventory delta) — feeds own-player prediction.
    mods: PhysicsMods,
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
    inv_ui: InventoryUi,
    craft_ui: CraftingUi,
    /// Mirror of the open chest (panel shows while `Some`).
    chest: Option<ChestMirror>,
    /// Server-authoritative own vitals (§8), mirrored for the HUD:
    /// `PlayerHealth` / `PlayerBreath` deltas. Also drives the Nurse cost
    /// preview (§7.4).
    hp: u16,
    max_hp: u16,
    breath: u16,
    /// Own active debuffs — the `PlayerDebuffs` replacement list plus its
    /// arrival time, so the HUD timers and the §10 Darkness light dimming
    /// count the synced ticks down locally (the Nurse preview reads it too).
    debuffs: Vec<ActiveDebuff>,
    debuffs_at: f64,
    /// Dead (§8): set by `PlayerDied`, cleared by `PlayerRespawned`. Gates
    /// movement and world interaction — the server drops both from corpses.
    dead: bool,
    died_at: f64,
    /// World progress flags (bosses defeated): Nurse cost preview (§7.4).
    flags: WorldFlags,
    /// Town NPC mirror + the dialogue/shop UI (§7).
    npcs: Npcs,
    dialogue: DialogueUi,
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
            mods: PhysicsMods::NONE,
            selected: 0,
            interact: Interact::new(),
            entities: Entities::new(),
            batch: QuadBatch::new(),
            sources: Vec::new(),
            remote_samples: Vec::new(),
            corners: Vec::new(),
            light_ms: 0.0,
            light_recomputes: 0,
            inv_ui: InventoryUi::new(),
            craft_ui: CraftingUi::new(),
            chest: None,
            hp: PLAYER_BASE_MAX_HP as u16,
            max_hp: PLAYER_BASE_MAX_HP as u16,
            breath: PLAYER_MAX_BREATH as u16,
            debuffs: Vec::new(),
            debuffs_at: 0.0,
            dead: false,
            died_at: f64::NEG_INFINITY,
            flags: welcome.flags,
            npcs: Npcs::new(),
            dialogue: DialogueUi::new(),
        }
    }

    /// Whether the own player's Darkness debuff (§5.2) is still running —
    /// halves the own light emission (§10).
    fn has_darkness(&self, now: f64) -> bool {
        self.debuffs.iter().any(|d| {
            d.debuff == Debuff::Darkness
                && d.remaining_ticks as f64 / TICK_RATE as f64 > now - self.debuffs_at
        })
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
        let chat_was_open = self.chat.open;
        if let Some(text) = self.chat.handle_input() {
            self.ws.send(&ClientMessage::Chat { text });
        }
        // Dead players don't move (the server drops their `PlayerState`s).
        let input = if self.chat.open || self.dead {
            PlayerInput::default()
        } else {
            gather_input()
        };
        if !chat_was_open {
            self.inventory_keys();
        }

        // 3. Own-player prediction at a fixed 60 Hz, frozen until the chunk
        // under us has streamed in (and while dead — the body is gone).
        // Equipment modifiers (Swift Boots, Gust Jar...) come from the
        // synced inventory via the shared loadout fn, so the server agrees
        // with what we predict.
        let center = self.own.phys.center();
        let frozen = self.dead || !self.view.chunk_loaded_at(center.0, center.1);
        for msg in self
            .own
            .update(self.view.world(), input, dt, frozen, self.mods)
        {
            self.ws.send(&msg);
        }

        // Chest follows reach (§11): walking away closes it on both sides.
        // A chest that stopped existing (someone broke it) closes too — the
        // server already released its lock with the break.
        let center = self.own.phys.center();
        if let Some(c) = &self.chest {
            let broken = self.view.world().tile(c.origin.0, c.origin.1).id != TileId::Chest;
            if broken || !chest_in_reach(center, c.origin) {
                self.chest = None;
                self.ws.send(&ClientMessage::CloseChest);
            }
        }

        // The dialogue panel follows talk range the same way (§7): walking
        // away from the NPC (or the NPC despawning) closes it.
        if let Some(npc_id) = self.dialogue.npc_id() {
            if !self.npcs.in_reach(npc_id, center, now - INTERP_DELAY) {
                self.dialogue.close();
            }
        }

        // 4. Clock & camera.
        self.clock.advance(dt);
        let center = self.own.phys.center();
        self.camera.follow(
            vec2(center.0, center.1) * TILE_SIZE,
            world_px(&self.view),
            dt,
        );

        // 4.5. Mouse world interaction (mining/placing/combat/doors/chests)
        // with the fresh camera. Quiet while chat owns the keyboard, the
        // inventory screen / dialogue panel owns the mouse (slot clicks and
        // Heal/Shop-button clicks must not swing or place), or we're dead
        // (§8: the dispatch gate drops world intents anyway). A right-click
        // on a hovered in-range NPC talks instead (§7.4 "Right-click →
        // Heal" is this path on the Nurse) and outranks the tile behind it.
        let tl = self.camera.top_left();
        let aim = Interact::aim(self.view.world(), tl);
        if !self.chat.open && !self.inv_ui.open && !self.dialogue.is_open() && !self.dead {
            let (mx, my) = mouse_position();
            let cursor = ((mx + tl.x) / TILE_SIZE, (my + tl.y) / TILE_SIZE);
            let hovered_npc = self
                .npcs
                .hovered_in_reach(cursor, center, now - INTERP_DELAY);
            if let Some(npc_id) = hovered_npc {
                if is_mouse_button_pressed(MouseButton::Right) {
                    self.ws.send(&ClientMessage::TalkNpc { npc_id });
                }
            }
            self.interact.frame(
                &self.ws,
                self.view.world(),
                center,
                &self.inventory,
                self.selected,
                aim,
                Interact::mouse_world(tl),
                hovered_npc.is_some(),
                now,
                dt,
            );
        }

        // 5. Lighting. Sample remote players once (lighting and drawing
        // share the snapshots), gather this frame's dynamic sources, and let
        // the engine decide whether the visible region needs a recompute.
        let render_t = now - INTERP_DELAY;
        self.remote_samples.clear();
        for (&id, remote) in self.remotes.iter_mut() {
            if remote.dead {
                continue; // hidden until PlayerRespawned (§8)
            }
            self.remote_samples.push((id, remote.sample(render_t)));
        }
        // Stable order: dynamic sources are compared against last frame's to
        // skip recomputes, which map iteration order must not defeat.
        self.remote_samples.sort_by_key(|&(id, _)| id);
        let ticks = self.clock.ticks();
        self.gather_light_sources(now);
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
        self.npcs.draw(render_t, now, tl, &self.light, center);
        for &(id, s) in &self.remote_samples {
            let Some(remote) = self.remotes.get(&id) else {
                continue;
            };
            let px = vec2(s.pos.0 * TILE_SIZE - tl.x, s.pos.1 * TILE_SIZE - tl.y);
            render::draw_player(&PlayerDraw {
                pos: px,
                world_x: s.pos.0,
                vel_x: s.vel.0,
                facing: s.facing,
                anim: s.anim,
                name: &remote.name,
                is_self: false,
                held_item: remote.held_item,
                swing: None,
                light: self
                    .light
                    .brightness_at(s.pos.0 + PLAYER_WIDTH / 2.0, s.pos.1 + PLAYER_HEIGHT / 2.0),
            });
            // Hurt teammates get the same floating bar as enemies, above
            // their name label.
            if remote.hp < remote.max_hp {
                crate::entities::draw_health_bar(
                    px.x + PLAYER_WIDTH * TILE_SIZE / 2.0,
                    px.y - 26.0,
                    remote.hp as f32 / remote.max_hp.max(1) as f32,
                    1.0,
                );
            }
        }
        let own_center = self.own.phys.center();
        if !self.dead {
            let p = &self.own.phys;
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
                swing: self.interact.swing(now),
                light: self.light.brightness_at(own_center.0, own_center.1),
            });
            self.interact.draw_aim(aim, center, tl);
        }
        // Night vignette, surface only (skip when buried — caves are dark
        // everywhere, the edges shouldn't be extra-dark).
        let own_tile = (own_center.0 as u32, own_center.1 as u32);
        if self.light.sky_exposed(own_tile.0, own_tile.1) {
            render::draw_vignette(1.0 - light::daylight(ticks), &mut self.batch);
        }

        // 7. Overlay UI: HUD (top-left status + top-right §8 survival
        // block), then the inventory screen (hotbar always; full panels +
        // crafting + chest while open), the death overlay, then chat on
        // top. World right-clicks (doors, chests) are `Interact`'s job in
        // step 4.5.
        self.hud
            .draw(self.remotes.len() + 1, self.clock.day, self.clock.ticks());
        self.hud.draw_survival(
            self.hp,
            self.max_hp,
            self.breath,
            &self.debuffs,
            self.debuffs_at,
            now,
        );
        let mut ui_msgs: Vec<ClientMessage> = Vec::new();
        let sell = self.dialogue.sell_target();
        // A drag-drop that lands a carried stack spends its LMB press: the
        // dialogue/shop UI below must ignore it (a sell drop on the shop
        // window would otherwise also buy the cell under the cursor).
        let click_spent = self.inv_ui.frame(
            &self.inventory,
            self.chest.as_ref(),
            sell.as_ref(),
            self.selected,
            &mut ui_msgs,
        );
        if self.inv_ui.open {
            let stations = stations_in_range(self.view.world(), self.own.phys.center());
            self.craft_ui
                .frame(&self.inventory, stations, now, &mut ui_msgs);
        }
        // NPC dialogue panel + merchant shop window (§7.3–§7.5).
        let shop_opened = self.dialogue.frame(
            &self.inventory,
            self.hp,
            self.max_hp,
            &self.debuffs,
            self.flags,
            now,
            click_spent,
            &mut ui_msgs,
        );
        if shop_opened {
            // The shop window sits where the chest panel would: show the
            // inventory beside it and let go of any open chest.
            self.inv_ui.open = true;
            if self.chest.take().is_some() {
                self.ws.send(&ClientMessage::CloseChest);
            }
        }
        for msg in ui_msgs {
            self.ws.send(&msg);
        }
        // Death screen (§8): dim everything, offer Respawn once the 10 s
        // timer (mirrored client-side) has run. The server gates the intent
        // too, so an early click is merely ignored.
        if self.dead {
            let secs_left = RESPAWN_SECS as f64 - (now - self.died_at);
            if ui::draw_death_screen(secs_left) {
                self.ws.send(&ClientMessage::Respawn);
            }
        }
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
    /// own player's head. A Darkness victim's emission is halved
    /// (§5.2/§10: light radius halved — flood-fill radius scales with the
    /// source level), own and remote alike. Dead players emit nothing.
    /// Remote helmets aren't knowable yet — the server doesn't broadcast
    /// worn armor (only held items); if that lands, hook it in here.
    fn gather_light_sources(&mut self, now: f64) {
        self.sources.clear();
        if !self.dead {
            let dark = self.has_darkness(now);
            let center = self.own.phys.center();
            self.sources.push(DynamicSource {
                x: center.0 as i32,
                y: center.1 as i32,
                level: dimmed(PLAYER_GLOW, dark),
            });
            // Specifically the head slot (§10 ties the light to *wearing*
            // the helmet): inventory slot `ARMOR_START + k`, in `ArmorSlot`
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
                    level: dimmed(MINING_HELMET_LIGHT, dark),
                });
            }
        }
        for &(id, s) in &self.remote_samples {
            let dark = self.remotes.get(&id).is_some_and(|r| r.has_darkness(now));
            self.sources.push(DynamicSource {
                x: (s.pos.0 + PLAYER_WIDTH / 2.0) as i32,
                y: (s.pos.1 + PLAYER_HEIGHT / 2.0) as i32,
                level: dimmed(PLAYER_GLOW, dark),
            });
        }
    }

    /// Inventory/dialogue keys (chat closed): E closes an open dialogue,
    /// else toggles the inventory (talking to an NPC is a right-click on
    /// them, step 4.5); Esc closes whatever is open; digits and the wheel
    /// drive the hotbar.
    fn inventory_keys(&mut self) {
        if is_key_pressed(KeyCode::E) {
            if self.dialogue.is_open() {
                // Single-line dialogue: "advance" closes the panel.
                self.dialogue.close();
            } else if self.inv_ui.open {
                self.close_inventory();
            } else {
                self.inv_ui.open = true;
            }
        } else if is_key_pressed(KeyCode::Escape) {
            if self.dialogue.is_open() {
                self.dialogue.close();
            } else if self.inv_ui.open {
                self.close_inventory();
            }
        }

        const DIGITS: [KeyCode; 10] = [
            KeyCode::Key1,
            KeyCode::Key2,
            KeyCode::Key3,
            KeyCode::Key4,
            KeyCode::Key5,
            KeyCode::Key6,
            KeyCode::Key7,
            KeyCode::Key8,
            KeyCode::Key9,
            KeyCode::Key0,
        ];
        let mut selected = self.selected;
        for (i, key) in DIGITS.iter().enumerate() {
            if is_key_pressed(*key) {
                selected = i as u8;
            }
        }
        // Wheel cycles the hotbar while the crafting list isn't using it.
        if !self.inv_ui.open {
            let (_, wheel) = mouse_wheel();
            let n = inventory::HOTBAR as i16;
            if wheel < 0.0 {
                selected = ((selected as i16 + 1) % n) as u8;
            } else if wheel > 0.0 {
                selected = ((selected as i16 + n - 1) % n) as u8;
            }
        }
        if selected != self.selected {
            self.selected = selected;
            self.ws.send(&ClientMessage::SelectSlot { slot: selected });
        }
    }

    fn close_inventory(&mut self) {
        self.inv_ui.close();
        if self.chest.take().is_some() {
            self.ws.send(&ClientMessage::CloseChest);
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
            // Inventory deltas refresh the derived state too: physics mods
            // (Swift Boots...) for prediction, the crafting panel's
            // optimistic flash, and — via the armor mirror — lighting (a
            // worn Mining Helmet is a light source, §10).
            ServerMessage::InventorySync { slots } => {
                self.inventory = slots;
                self.inventory.resize(inventory::TOTAL, None);
                self.mods = loadout::physics_mods(&self.inventory);
            }
            ServerMessage::SlotChanged { idx, stack } => {
                if let Some(slot) = self.inventory.get_mut(idx as usize) {
                    *slot = stack;
                }
                self.mods = loadout::physics_mods(&self.inventory);
                self.craft_ui.reconcile();
                self.dialogue.reconcile();
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
            ServerMessage::EntityUpdate { entities } => {
                // NPCs and generic entities share the id space; each mirror
                // ignores ids it doesn't track.
                self.npcs.update(&entities, now);
                self.entities.update(&entities, now);
            }
            ServerMessage::EntityHurt { id, damage, crit } => {
                self.entities.on_hurt(id, damage, crit, now)
            }
            ServerMessage::EntityDespawn { id, reason } => {
                self.entities.on_despawn(id, reason, now);
                self.npcs.remove(id);
                if self.dialogue.npc_id() == Some(id) {
                    self.dialogue.close();
                }
            }
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
            ServerMessage::PlayerHealth { id, hp, max_hp } => {
                if id == self.own_id {
                    self.hp = hp;
                    self.max_hp = max_hp;
                } else if let Some(remote) = self.remotes.get_mut(&id) {
                    remote.hp = hp;
                    remote.max_hp = max_hp;
                }
            }
            ServerMessage::PlayerBreath { id, breath } => {
                if id == self.own_id {
                    self.breath = breath;
                }
            }
            ServerMessage::PlayerDebuffs { id, debuffs } => {
                if id == self.own_id {
                    self.debuffs = debuffs;
                    self.debuffs_at = now;
                } else if let Some(remote) = self.remotes.get_mut(&id) {
                    remote.set_debuffs(&debuffs, now);
                }
            }
            // Movement is client-authoritative, so the server can't shove
            // us itself — it asks (protocol.rs), and we add the impulse to
            // the predicted velocity.
            ServerMessage::PlayerKnockback { vx, vy } => self.own.apply_knockback(vx, vy),
            ServerMessage::PlayerDied { id } => {
                if id == self.own_id {
                    self.dead = true;
                    self.died_at = now;
                } else if let Some(remote) = self.remotes.get_mut(&id) {
                    remote.dead = true; // hidden until PlayerRespawned
                }
            }
            ServerMessage::PlayerRespawned { id, pos } => {
                if id == self.own_id {
                    self.dead = false;
                    // The server silently restored the breath meter (§8);
                    // debuffs were already cleared via PlayerDebuffs.
                    self.breath = PLAYER_MAX_BREATH as u16;
                    // Adopt the respawn point outright and recenter the
                    // camera, mirroring the own-id `PlayerMoved` correction
                    // snap — smoothing would pan across unloaded world.
                    self.own.apply_correction(pos, (0.0, 0.0));
                    let center = self.own.phys.center();
                    let bounds = world_px(&self.view);
                    self.camera
                        .snap(vec2(center.0, center.1) * TILE_SIZE, bounds);
                } else if let Some(remote) = self.remotes.get_mut(&id) {
                    remote.dead = false;
                    remote.reset_to(pos, now);
                }
            }
            ServerMessage::Chat { from, text } => self.chat.push_message(&from, &text, now),
            ServerMessage::Toast { text } => self.chat.push_system(text, now),
            ServerMessage::TimeSync { time, day } => self.clock.set(time, day),
            ServerMessage::ChestContents { x, y, slots } => {
                self.chest = Some(ChestMirror::new((x, y), slots));
                self.inv_ui.open = true; // chest panel sits beside the inventory
            }
            ServerMessage::ChestSlotChanged { idx, stack } => {
                if let Some(c) = &mut self.chest {
                    if let Some(slot) = c.slots.get_mut(idx as usize) {
                        *slot = stack;
                    }
                }
            }
            ServerMessage::ChestDenied => {
                self.chat
                    .push_system("Someone else is using that chest".into(), now);
            }
            ServerMessage::NpcList { npcs } => {
                self.npcs.apply_list(npcs, now);
                if let Some(npc_id) = self.dialogue.npc_id() {
                    if self.npcs.get(npc_id).is_none() {
                        self.dialogue.close();
                    }
                }
            }
            ServerMessage::NpcDialogue { npc_id, line } => {
                if let Some(npc) = self.npcs.get(npc_id) {
                    self.dialogue
                        .open_line(npc_id, npc.kind, npc.name.clone(), line);
                }
            }
            ServerMessage::ShopContents { npc_id, items } => {
                self.dialogue.set_shop(npc_id, items);
            }
            ServerMessage::WorldFlags { flags } => self.flags = flags,
            // Session-layer messages, consumed before `apply` (`Reject`/
            // `Welcome`) or by the ping loop (`Pong`). The match is
            // exhaustive on purpose: a new protocol variant must be wired
            // (or parked here) explicitly.
            ServerMessage::Reject { .. }
            | ServerMessage::Welcome { .. }
            | ServerMessage::Pong { .. } => {}
        }
    }
}

/// §10/§5.2: a Darkness victim's light emission is halved (flood-fill
/// radius scales with the source level, so halving the level halves the
/// radius).
fn dimmed(level: u8, darkness: bool) -> u8 {
    if darkness {
        (level as f32 * DARKNESS_LIGHT_RADIUS_MULT) as u8
    } else {
        level
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
