//! 2D overlay UI: menu / connecting / disconnected screens, the in-game HUD
//! (player count, clock, F3 debug), chat, and the inventory screen
//! ([`inventory`]: hotbar/backpack/equipment/chest panels; [`crafting`]:
//! the crafting panel).
//!
//! Per-frame string allocation is avoided: the HUD caches its text and only
//! reformats when the underlying value changes, chat lines are formatted
//! once on arrival, and the debug overlay refreshes on a slow timer.
//! (Tooltips allocate while hovering — one small panel at most.)

pub mod crafting;
pub mod dialogue;
pub mod inventory;

use std::collections::VecDeque;

use macroquad::prelude::*;

use ferraria_shared::protocol::{ActiveDebuff, Debuff};
use ferraria_shared::{CHAT_MAX_CHARS, LIFE_CRYSTAL_HP, PLAYER_MAX_BREATH, TICK_RATE};

const TITLE: &str = "FERRARIA";

const TEXT_COLOR: Color = Color::new(0.92, 0.92, 0.95, 1.0);
const DIM_TEXT: Color = Color::new(0.65, 0.65, 0.72, 1.0);
const ERROR_COLOR: Color = Color::new(0.95, 0.45, 0.40, 1.0);
const PANEL_BG: Color = Color::new(0.0, 0.0, 0.0, 0.45);
const BUTTON_BG: Color = Color::new(0.18, 0.22, 0.32, 1.0);
const BUTTON_BG_HOVER: Color = Color::new(0.28, 0.34, 0.48, 1.0);

/// Chat lines shown at once, and their fade timing (visible 10 s, then a
/// 1 s fade-out).
const CHAT_SHOW_LINES: usize = 6;
const CHAT_VISIBLE_SECS: f64 = 10.0;
const CHAT_FADE_SECS: f64 = 1.0;
const CHAT_KEEP_LINES: usize = 32;
const CHAT_FONT: f32 = 20.0;

/// Debug overlay refresh period (avoids reformatting strings every frame).
const DEBUG_REFRESH_SECS: f64 = 0.25;

// ---- Small helpers -----------------------------------------------------------------

pub fn shadow_text(text: &str, x: f32, y: f32, size: f32, color: Color) {
    draw_text(text, x + 1.0, y + 1.0, size, Color::new(0.0, 0.0, 0.0, 0.6));
    draw_text(text, x, y, size, color);
}

/// Draws a clickable button; true when clicked this frame. The click is also
/// the browser's audio-unlock user gesture (docs/NETWORKING.md).
pub fn button(x: f32, y: f32, w: f32, h: f32, label: &str) -> bool {
    let (mx, my) = mouse_position();
    let hover = mx >= x && mx <= x + w && my >= y && my <= y + h;
    draw_rectangle(x, y, w, h, if hover { BUTTON_BG_HOVER } else { BUTTON_BG });
    draw_rectangle_lines(x, y, w, h, 2.0, Color::new(1.0, 1.0, 1.0, 0.25));
    let dims = measure_text(label, None, 26, 1.0);
    shadow_text(
        label,
        x + (w - dims.width) * 0.5,
        y + h * 0.5 + dims.height * 0.4,
        26.0,
        TEXT_COLOR,
    );
    hover && is_mouse_button_pressed(MouseButton::Left)
}

/// Feeds this frame's typed characters into `buf` (printables only, capped),
/// handling backspace. Drives both the name field and chat input.
pub fn text_input(buf: &mut String, max_chars: usize) {
    while let Some(c) = get_char_pressed() {
        if !c.is_control() && buf.chars().count() < max_chars {
            buf.push(c);
        }
    }
    if is_key_pressed(KeyCode::Backspace) {
        buf.pop();
    }
}

/// Discards any characters typed this frame (keeps the queue from leaking
/// into the chat box when it opens).
pub fn discard_typed_chars() {
    while get_char_pressed().is_some() {}
}

// ---- Full-screen states --------------------------------------------------------------

/// Title + name entry. Returns true when the player submits (Join click —
/// the audio-unlock gesture — or Enter, handled by the caller).
pub fn draw_menu(name: &str, error: Option<&str>) -> bool {
    clear_background(Color::new(0.07, 0.08, 0.13, 1.0));
    let cx = screen_width() * 0.5;
    let cy = screen_height() * 0.42;

    let dims = measure_text(TITLE, None, 96, 1.0);
    shadow_text(TITLE, cx - dims.width * 0.5, cy - 90.0, 96.0, TEXT_COLOR);

    shadow_text("Enter your name:", cx - 160.0, cy - 14.0, 24.0, DIM_TEXT);
    // Name field.
    let (fw, fh) = (320.0, 40.0);
    draw_rectangle(cx - fw * 0.5, cy, fw, fh, PANEL_BG);
    draw_rectangle_lines(
        cx - fw * 0.5,
        cy,
        fw,
        fh,
        2.0,
        Color::new(1.0, 1.0, 1.0, 0.3),
    );
    shadow_text(name, cx - fw * 0.5 + 10.0, cy + 28.0, 28.0, TEXT_COLOR);
    if get_time() % 1.0 < 0.5 {
        let w = measure_text(name, None, 28, 1.0).width;
        draw_rectangle(cx - fw * 0.5 + 12.0 + w, cy + 10.0, 3.0, 22.0, TEXT_COLOR);
    }

    if let Some(err) = error {
        let w = measure_text(err, None, 22, 1.0).width;
        shadow_text(err, cx - w * 0.5, cy + 70.0, 22.0, ERROR_COLOR);
    }

    shadow_text(
        "A/D or arrows to move, Space to jump, S+Space to drop, Enter to chat, F3 debug",
        cx - 290.0,
        screen_height() - 30.0,
        18.0,
        DIM_TEXT,
    );

    button(cx - 80.0, cy + 90.0, 160.0, 48.0, "Join")
}

/// Spinner-ish HUD while the socket/handshake is in flight.
pub fn draw_connecting(elapsed: f64) {
    clear_background(Color::new(0.07, 0.08, 0.13, 1.0));
    let dots = ".".repeat(1 + (elapsed * 2.0) as usize % 3);
    let cx = screen_width() * 0.5;
    shadow_text(
        "Connecting to server",
        cx - 130.0,
        screen_height() * 0.5,
        28.0,
        TEXT_COLOR,
    );
    shadow_text(&dots, cx + 128.0, screen_height() * 0.5, 28.0, TEXT_COLOR);
}

/// Reason + reconnect/menu buttons. Returns what the player clicked.
pub enum DisconnectedChoice {
    None,
    Reconnect,
    Menu,
}

/// Full-screen §8 death overlay drawn over the (still live) world: a dim,
/// the death text, and the respawn control — a countdown while the server's
/// §8 timer (10 s) runs, then a button. Returns true on a Respawn click.
/// Per-frame formatting is fine here, like [`draw_connecting`] — this only
/// runs while dead.
pub fn draw_death_screen(secs_left: f64) -> bool {
    draw_rectangle(
        0.0,
        0.0,
        screen_width(),
        screen_height(),
        Color::new(0.08, 0.0, 0.0, 0.55),
    );
    let cx = screen_width() * 0.5;
    let cy = screen_height() * 0.42;
    let w = measure_text("You died", None, 64, 1.0).width;
    shadow_text("You died", cx - w * 0.5, cy, 64.0, ERROR_COLOR);
    if secs_left > 0.0 {
        let msg = format!("Respawn in {}s", secs_left.ceil() as u32);
        let mw = measure_text(&msg, None, 26, 1.0).width;
        shadow_text(&msg, cx - mw * 0.5, cy + 56.0, 26.0, TEXT_COLOR);
        false
    } else {
        button(cx - 90.0, cy + 28.0, 180.0, 48.0, "Respawn")
    }
}

pub fn draw_disconnected(reason: &str) -> DisconnectedChoice {
    clear_background(Color::new(0.10, 0.06, 0.07, 1.0));
    let cx = screen_width() * 0.5;
    let cy = screen_height() * 0.42;
    let w = measure_text("Disconnected", None, 48, 1.0).width;
    shadow_text("Disconnected", cx - w * 0.5, cy - 40.0, 48.0, ERROR_COLOR);
    let rw = measure_text(reason, None, 24, 1.0).width;
    shadow_text(reason, cx - rw * 0.5, cy + 4.0, 24.0, TEXT_COLOR);
    if button(cx - 170.0, cy + 50.0, 160.0, 44.0, "Reconnect") {
        return DisconnectedChoice::Reconnect;
    }
    if button(cx + 10.0, cy + 50.0, 160.0, 44.0, "Menu") {
        return DisconnectedChoice::Menu;
    }
    DisconnectedChoice::None
}

// ---- HUD --------------------------------------------------------------------------

/// Survival HUD layout (§8), top-right: one heart per 20 max HP (Life
/// Crystals add hearts, 400 max HP = 20), rows of 10; a breath bar shown
/// only while the meter isn't full; debuff badges with remaining time.
const HEART_PX: f32 = 15.0;
const HEART_GAP: f32 = 4.0;
const HEARTS_PER_ROW: usize = 10;
const HUD_RIGHT_MARGIN: f32 = 12.0;
const HUD_TOP: f32 = 14.0;
const HEART_BG: Color = Color::new(0.20, 0.04, 0.06, 0.80);
const HEART_FILL: Color = Color::new(0.88, 0.18, 0.25, 0.95);
const BREATH_BG: Color = Color::new(0.02, 0.08, 0.16, 0.80);
const BREATH_FILL: Color = Color::new(0.35, 0.75, 0.95, 0.95);
const BREATH_H: f32 = 7.0;

/// Display name + badge color of a §8 debuff.
fn debuff_style(d: Debuff) -> (&'static str, Color) {
    match d {
        Debuff::Burning => ("Burning", Color::new(1.0, 0.55, 0.15, 1.0)),
        Debuff::Darkness => ("Darkness", Color::new(0.55, 0.35, 0.85, 1.0)),
        Debuff::PotionSickness => ("Potion Sickness", Color::new(0.85, 0.45, 0.75, 1.0)),
    }
}

/// Top-left status block, the top-right §8 survival block, + F3 debug
/// overlay, with cached strings.
pub struct Hud {
    pub debug: bool,
    clock_text: String,
    clock_key: (u32, u32),
    players_text: String,
    players_key: usize,
    hp_text: String,
    hp_key: (u16, u16),
    /// Debuff badge labels, rebuilt only when a displayed second changes.
    debuff_labels: Vec<(Debuff, u32, String)>,
    debuff_key: Vec<(Debuff, u32)>,
    debug_lines: Vec<String>,
    debug_at: f64,
}

impl Hud {
    pub fn new() -> Hud {
        Hud {
            debug: false,
            clock_text: String::new(),
            clock_key: (u32::MAX, u32::MAX),
            players_text: String::new(),
            players_key: usize::MAX,
            hp_text: String::new(),
            hp_key: (u16::MAX, u16::MAX),
            debuff_labels: Vec::new(),
            debuff_key: Vec::new(),
            debug_lines: Vec::new(),
            debug_at: f64::NEG_INFINITY,
        }
    }

    pub fn draw(&mut self, players: usize, day: u32, time: u32) {
        if players != self.players_key {
            self.players_key = players;
            self.players_text = format!("Players: {players}");
        }
        let minute = time / 60;
        if (day, minute) != self.clock_key {
            self.clock_key = (day, minute);
            self.clock_text = format_clock(day, time);
        }
        shadow_text(&self.players_text, 12.0, 24.0, 22.0, TEXT_COLOR);
        shadow_text(&self.clock_text, 12.0, 48.0, 22.0, TEXT_COLOR);
    }

    /// Top-right §8 survival block: hearts, "hp/max", the breath bar (only
    /// while below the 200 max), and debuff badges with remaining time
    /// (ticks synced by `PlayerDebuffs` at `debuffs_at`, counted down
    /// locally).
    pub fn draw_survival(
        &mut self,
        hp: u16,
        max_hp: u16,
        breath: u16,
        debuffs: &[ActiveDebuff],
        debuffs_at: f64,
        now: f64,
    ) {
        let sw = screen_width();
        // Hearts: one per 20 max HP (§8: 100 base + 20/Life Crystal).
        let hearts = (max_hp as u32).div_ceil(LIFE_CRYSTAL_HP).max(1) as usize;
        let rows = hearts.div_ceil(HEARTS_PER_ROW);
        for i in 0..hearts {
            let row = i / HEARTS_PER_ROW;
            let col = i % HEARTS_PER_ROW;
            let row_len = (hearts - row * HEARTS_PER_ROW).min(HEARTS_PER_ROW);
            let x = sw - HUD_RIGHT_MARGIN - ((row_len - col) as f32) * (HEART_PX + HEART_GAP)
                + HEART_GAP;
            let y = HUD_TOP + row as f32 * (HEART_PX + HEART_GAP);
            draw_rectangle(x, y, HEART_PX, HEART_PX, HEART_BG);
            let fill = (hp as i32 - (i as u32 * LIFE_CRYSTAL_HP) as i32)
                .clamp(0, LIFE_CRYSTAL_HP as i32) as f32
                / LIFE_CRYSTAL_HP as f32;
            if fill > 0.0 {
                draw_rectangle(
                    x,
                    y + HEART_PX * (1.0 - fill),
                    HEART_PX,
                    HEART_PX * fill,
                    HEART_FILL,
                );
            }
            draw_rectangle_lines(
                x,
                y,
                HEART_PX,
                HEART_PX,
                1.0,
                Color::new(1.0, 1.0, 1.0, 0.25),
            );
        }
        let mut next_y = HUD_TOP + rows as f32 * (HEART_PX + HEART_GAP) + 12.0;
        if (hp, max_hp) != self.hp_key {
            self.hp_key = (hp, max_hp);
            self.hp_text = format!("{hp}/{max_hp}");
        }
        let dims = measure_text(&self.hp_text, None, 18, 1.0);
        shadow_text(
            &self.hp_text,
            sw - HUD_RIGHT_MARGIN - dims.width,
            next_y,
            18.0,
            DIM_TEXT,
        );
        next_y += 12.0;

        // Breath (§8: 200 max), shown only while not full.
        if (breath as u32) < PLAYER_MAX_BREATH {
            let bw = HEARTS_PER_ROW as f32 * (HEART_PX + HEART_GAP) - HEART_GAP;
            let x = sw - HUD_RIGHT_MARGIN - bw;
            draw_rectangle(x, next_y, bw, BREATH_H, BREATH_BG);
            draw_rectangle(
                x,
                next_y,
                bw * breath as f32 / PLAYER_MAX_BREATH as f32,
                BREATH_H,
                BREATH_FILL,
            );
            next_y += BREATH_H + 10.0;
        } else {
            next_y += 4.0;
        }

        // Debuff badges (§8: Burning, Darkness, Potion Sickness). Labels
        // reformat only when a displayed second changes.
        self.debuff_key.clear();
        for d in debuffs {
            let left = d.remaining_ticks as f64 / TICK_RATE as f64 - (now - debuffs_at);
            if left > 0.0 {
                self.debuff_key.push((d.debuff, left.ceil() as u32));
            }
        }
        let stale = self.debuff_labels.len() != self.debuff_key.len()
            || self
                .debuff_labels
                .iter()
                .zip(&self.debuff_key)
                .any(|(l, k)| (l.0, l.1) != *k);
        if stale {
            self.debuff_labels = self
                .debuff_key
                .iter()
                .map(|&(d, s)| (d, s, format!("{} {}s", debuff_style(d).0, s)))
                .collect();
        }
        for (d, _, label) in &self.debuff_labels {
            let (_, color) = debuff_style(*d);
            let dims = measure_text(label, None, 18, 1.0);
            let tx = sw - HUD_RIGHT_MARGIN - dims.width;
            draw_rectangle(tx - 17.0, next_y, 12.0, 12.0, color);
            shadow_text(label, tx, next_y + 11.0, 18.0, TEXT_COLOR);
            next_y += 19.0;
        }
    }

    /// F3 overlay: fps, position, chunk count, bad frames, light timings.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_debug(
        &mut self,
        now: f64,
        pos: (f32, f32),
        vel: (f32, f32),
        chunks: usize,
        bad_frames: u32,
        light_ms: f64,
        light_recomputes: u64,
    ) {
        if now - self.debug_at >= DEBUG_REFRESH_SECS {
            self.debug_at = now;
            self.debug_lines.clear();
            self.debug_lines.push(format!("fps: {}", get_fps()));
            self.debug_lines
                .push(format!("pos: ({:.2}, {:.2}) tiles", pos.0, pos.1));
            self.debug_lines
                .push(format!("vel: ({:.2}, {:.2}) t/s", vel.0, vel.1));
            self.debug_lines.push(format!("chunks: {chunks}"));
            self.debug_lines.push(format!("bad frames: {bad_frames}"));
            self.debug_lines.push(format!(
                "light: {light_ms:.2} ms last recompute ({light_recomputes} total)"
            ));
        }
        for (i, line) in self.debug_lines.iter().enumerate() {
            shadow_text(line, 12.0, 78.0 + i as f32 * 20.0, 18.0, TEXT_COLOR);
        }
    }
}

impl Default for Hud {
    fn default() -> Self {
        Hud::new()
    }
}

/// "Day 3 - 8:15 AM" — `time` is the tick-of-day (1 tick = 1 in-game
/// second, DESIGN §9), `day` the completed-day count.
fn format_clock(day: u32, time: u32) -> String {
    let hours = time / 3600;
    let minutes = (time % 3600) / 60;
    let (h12, ampm) = match hours {
        0 => (12, "AM"),
        1..=11 => (hours, "AM"),
        12 => (12, "PM"),
        _ => (hours - 12, "PM"),
    };
    format!("Day {} - {}:{:02} {}", day + 1, h12, minutes, ampm)
}

// ---- Chat -------------------------------------------------------------------------

struct ChatLine {
    /// Pre-formatted "name: text" (built once on arrival).
    text: String,
    at: f64,
}

/// Chat log + input line. Enter opens, Esc cancels, Enter again sends.
pub struct Chat {
    pub open: bool,
    input: String,
    lines: VecDeque<ChatLine>,
}

impl Chat {
    pub fn new() -> Chat {
        Chat {
            open: false,
            input: String::new(),
            lines: VecDeque::new(),
        }
    }

    pub fn push_message(&mut self, from: &str, text: &str, now: f64) {
        self.push_line(format!("{from}: {text}"), now);
    }

    /// Join/leave notices etc.
    pub fn push_system(&mut self, text: String, now: f64) {
        self.push_line(text, now);
    }

    fn push_line(&mut self, text: String, now: f64) {
        if self.lines.len() >= CHAT_KEEP_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(ChatLine { text, at: now });
    }

    /// Handles all chat keyboard interaction for the frame; returns a
    /// message to send when the player submits one. While open this consumes
    /// the character queue (the caller suppresses movement input).
    pub fn handle_input(&mut self) -> Option<String> {
        if self.open {
            text_input(&mut self.input, CHAT_MAX_CHARS);
            if is_key_pressed(KeyCode::Escape) {
                self.open = false;
                self.input.clear();
                return None;
            }
            if is_key_pressed(KeyCode::Enter) {
                self.open = false;
                let msg = std::mem::take(&mut self.input);
                let trimmed = msg.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            None
        } else {
            discard_typed_chars();
            if is_key_pressed(KeyCode::Enter) {
                self.open = true;
            }
            None
        }
    }

    /// Bottom-left log (last 6 lines, fading 10 s after arrival — always
    /// shown while the input is open) and the input line.
    pub fn draw(&self, now: f64) {
        let base_y = screen_height() - 60.0;
        let recent = self
            .lines
            .iter()
            .rev()
            .take(CHAT_SHOW_LINES)
            .collect::<Vec<_>>();
        for (i, line) in recent.iter().enumerate() {
            let age = now - line.at;
            let alpha = if self.open || age <= CHAT_VISIBLE_SECS {
                1.0
            } else if age <= CHAT_VISIBLE_SECS + CHAT_FADE_SECS {
                (1.0 - (age - CHAT_VISIBLE_SECS) / CHAT_FADE_SECS) as f32
            } else {
                continue;
            };
            let y = base_y - 22.0 * (i as f32 + 1.0);
            draw_text(
                &line.text,
                13.0,
                y + 1.0,
                CHAT_FONT,
                Color::new(0.0, 0.0, 0.0, 0.5 * alpha),
            );
            draw_text(
                &line.text,
                12.0,
                y,
                CHAT_FONT,
                Color::new(0.92, 0.92, 0.95, alpha),
            );
        }
        if self.open {
            draw_rectangle(8.0, base_y - 16.0, screen_width() * 0.5, 28.0, PANEL_BG);
            shadow_text(">", 14.0, base_y + 4.0, CHAT_FONT, TEXT_COLOR);
            shadow_text(&self.input, 28.0, base_y + 4.0, CHAT_FONT, TEXT_COLOR);
            if now % 1.0 < 0.5 {
                let w = measure_text(&self.input, None, CHAT_FONT as u16, 1.0).width;
                draw_rectangle(30.0 + w, base_y - 11.0, 2.0, 18.0, TEXT_COLOR);
            }
        }
    }
}

impl Default for Chat {
    fn default() -> Self {
        Chat::new()
    }
}
