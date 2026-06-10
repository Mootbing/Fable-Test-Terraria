//! The Ferraria browser client (wasm32 + macroquad).
//!
//! Module map:
//! - [`net`] — WebSocket facade over the `web/quad_ws.js` plugin.
//! - [`app`] — Menu → Connecting → Playing → Disconnected state machine and
//!   the live `Session`.
//! - [`world_view`] — mirror of the server world (chunks + tile deltas).
//! - [`player`] — own-player prediction, remote-player interpolation.
//! - [`entities`] — mirror of server entities (item drops, later enemies).
//! - [`interact`] — mouse aiming, mining/placing intents, crack overlay.
//! - [`light`] — client-side flood-fill lighting + day/night sky ramp.
//! - [`render`] — camera, sky, tiles, player sprites.
//! - [`ui`] — menus, HUD, chat, debug overlay (`ui::inventory`,
//!   `ui::crafting`: hotbar + inventory screen, crafting panel).

mod app;
mod entities;
mod interact;
mod light;
mod net;
mod player;
mod render;
mod ui;
mod world_view;

use macroquad::prelude::*;

fn conf() -> Conf {
    Conf {
        window_title: "Ferraria".to_string(),
        window_width: 1280,
        window_height: 720,
        high_dpi: true,
        ..Default::default()
    }
}

#[macroquad::main(conf)]
async fn main() {
    let mut app = app::App::new();
    loop {
        app.frame();
        next_frame().await;
    }
}
