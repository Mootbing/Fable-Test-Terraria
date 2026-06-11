//! NPC dialogue panel and the merchant shop window (DESIGN §7.3–§7.5).
//!
//! Bottom panel: NPC name + the server-picked line; E or a click on the
//! panel closes it. The Merchant panel adds a Shop button (grid of §7.3
//! stock with coin-formatted prices, click-to-buy with an optimistic grey
//! flash reconciled by `SlotChanged`); the Nurse panel adds a Heal button
//! with a live §7.4 cost preview computed from synced state via the same
//! shared formula the server charges with.
//!
//! Sell mode lives in `ui::inventory`: while the shop window is open it is
//! a [`SellTarget`] — dragging a stack onto it or shift-clicking an
//! inventory slot sends `SellItem`, and tooltips grow a sell-price line.

use macroquad::prelude::*;

use ferraria_shared::coins::coin_total;
use ferraria_shared::items::{InvSlot, ItemId};
use ferraria_shared::npc::nurse_heal_cost;
use ferraria_shared::protocol::{ActiveDebuff, ClientMessage, Debuff, NpcKind, ShopEntry};
use ferraria_shared::world::WorldFlags;

use super::inventory::{draw_item, format_coins, Rect, SellTarget, GAP, ORIGIN, SLOT};
use super::shadow_text;

const PANEL_BG: Color = Color::new(0.04, 0.05, 0.09, 0.92);
const PANEL_BORDER: Color = Color::new(1.0, 1.0, 1.0, 0.22);
const TEXT: Color = Color::new(0.92, 0.92, 0.95, 1.0);
const DIM: Color = Color::new(0.62, 0.62, 0.70, 1.0);
const NAME_COLOR: Color = Color::new(0.85, 0.95, 0.6, 1.0);
const PRICE_COLOR: Color = Color::new(0.95, 0.85, 0.45, 1.0);
const CANT_AFFORD: Color = Color::new(0.85, 0.45, 0.4, 1.0);
const BUTTON_BG: Color = Color::new(0.18, 0.22, 0.32, 1.0);
const BUTTON_BG_HOVER: Color = Color::new(0.28, 0.34, 0.48, 1.0);
const FLASH_SECS: f64 = 0.3;

/// The dialogue currently on screen.
pub struct ActiveDialogue {
    pub npc_id: u32,
    pub kind: NpcKind,
    pub name: String,
    pub line: String,
}

/// The §7.3 catalog received on talking to the Merchant.
struct Shop {
    npc_id: u32,
    items: Vec<ShopEntry>,
    /// Optimistic click feedback: entry flashes grey until the
    /// `SlotChanged` round-trip lands (or it times out).
    flash: Option<(ItemId, f64)>,
}

pub struct DialogueUi {
    active: Option<ActiveDialogue>,
    shop: Option<Shop>,
    /// The shop window is showing (toggled by the panel's Shop button).
    shop_open: bool,
}

impl DialogueUi {
    pub fn new() -> DialogueUi {
        DialogueUi {
            active: None,
            shop: None,
            shop_open: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    pub fn npc_id(&self) -> Option<u32> {
        self.active.as_ref().map(|a| a.npc_id)
    }

    /// `NpcDialogue` arrived (kind/name resolved from the roster mirror).
    pub fn open_line(&mut self, npc_id: u32, kind: NpcKind, name: String, line: String) {
        if self.npc_id() != Some(npc_id) {
            self.shop_open = false;
        }
        self.active = Some(ActiveDialogue {
            npc_id,
            kind,
            name,
            line,
        });
    }

    /// `ShopContents` arrived.
    pub fn set_shop(&mut self, npc_id: u32, items: Vec<ShopEntry>) {
        self.shop = Some(Shop {
            npc_id,
            items,
            flash: None,
        });
    }

    pub fn close(&mut self) {
        self.active = None;
        self.shop_open = false;
    }

    /// A `SlotChanged` landed: the buy round-trip is complete.
    pub fn reconcile(&mut self) {
        if let Some(shop) = &mut self.shop {
            shop.flash = None;
        }
    }

    /// The open shop window as a sell drop-target for `ui::inventory`'s
    /// drag/shift-click machinery.
    pub fn sell_target(&self) -> Option<SellTarget> {
        if !self.shop_open || !self.is_open() {
            return None;
        }
        let shop = self.shop.as_ref()?;
        Some(SellTarget {
            npc_id: shop.npc_id,
            rect: shop_window_rect(shop.items.len()),
        })
    }

    /// Draws the panel (and shop window). Returns `true` when the shop
    /// window was opened this frame (the caller opens the inventory screen
    /// beside it). Clicks append protocol messages to `out` — except when
    /// `click_spent` is set: the inventory UI (drawn earlier this frame)
    /// already consumed the LMB press dropping a carried stack, and e.g. a
    /// sell drop on the shop window must not also buy the cell under it.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        inv: &[Option<InvSlot>],
        own_hp: u16,
        own_max_hp: u16,
        debuffs: &[ActiveDebuff],
        flags: WorldFlags,
        now: f64,
        click_spent: bool,
        out: &mut Vec<ClientMessage>,
    ) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        let mouse = mouse_position();
        let mut shop_opened = false;

        // ---- Bottom panel. ---------------------------------------------------
        let pw = (screen_width() * 0.55).clamp(420.0, 760.0);
        let lines = wrap_text(&active.line, pw - 24.0, 20);
        let ph = 64.0 + lines.len() as f32 * 22.0;
        let px = (screen_width() - pw) * 0.5;
        let py = screen_height() - ph - 70.0;
        let panel = Rect {
            x: px,
            y: py,
            w: pw,
            h: ph,
        };
        draw_rectangle(px, py, pw, ph, PANEL_BG);
        draw_rectangle_lines(px, py, pw, ph, 2.0, PANEL_BORDER);
        shadow_text(&active.name, px + 12.0, py + 24.0, 22.0, NAME_COLOR);
        for (i, l) in lines.iter().enumerate() {
            draw_text(l, px + 12.0, py + 48.0 + i as f32 * 22.0, 20.0, TEXT);
        }
        shadow_text("[E] close", px + pw - 84.0, py + 20.0, 16.0, DIM);

        // ---- Role buttons. ------------------------------------------------------
        let by = py + ph - 34.0;
        let mut clicked_button = false;
        match active.kind {
            NpcKind::Merchant => {
                let label = if self.shop_open { "Close Shop" } else { "Shop" };
                if draw_button(px + 12.0, by, 110.0, 26.0, label, true, mouse) && !click_spent {
                    clicked_button = true;
                    self.shop_open = !self.shop_open;
                    shop_opened = self.shop_open;
                }
            }
            NpcKind::Nurse => {
                // Live §7.4 cost preview from synced state — the same
                // shared formula the server charges with.
                let hp_restored = own_max_hp.saturating_sub(own_hp);
                let cleared = debuffs
                    .iter()
                    .filter(|d| d.debuff != Debuff::PotionSickness)
                    .count() as u32;
                let (label, enabled) = if hp_restored == 0 {
                    ("Heal (healthy)".to_string(), false)
                } else {
                    let cost = nurse_heal_cost(
                        hp_restored as u32,
                        cleared,
                        flags.watcher_defeated,
                        flags.bone_warden_defeated,
                    );
                    let affordable = coin_total(inv) >= cost;
                    (format!("Heal ({})", format_coins(cost)), affordable)
                };
                if draw_button(px + 12.0, by, 170.0, 26.0, &label, enabled, mouse) && !click_spent {
                    clicked_button = true;
                    out.push(ClientMessage::NurseHeal);
                }
            }
            NpcKind::Sage => {}
        }

        // Click on the panel body (not a button) advances/closes it.
        if !clicked_button
            && !click_spent
            && is_mouse_button_pressed(MouseButton::Left)
            && panel.contains(mouse)
        {
            self.close();
            return false;
        }

        // ---- Shop window. ----------------------------------------------------------
        if self.shop_open {
            if let Some(shop) = &mut self.shop {
                if shop.flash.is_some_and(|(_, at)| now - at > FLASH_SECS) {
                    shop.flash = None;
                }
                draw_shop(shop, inv, mouse, now, click_spent, out);
            }
        }
        shop_opened
    }
}

impl Default for DialogueUi {
    fn default() -> Self {
        DialogueUi::new()
    }
}

/// Shop layout: 4 columns right of the equipment column (where the chest
/// panel would sit; the chest closes while a shop is open).
const SHOP_COLS: usize = 4;
const SHOP_CELL_W: f32 = SLOT + 64.0;
const SHOP_CELL_H: f32 = SLOT + 26.0;

fn shop_window_rect(n_items: usize) -> Rect {
    let rows = n_items.div_ceil(SHOP_COLS).max(1);
    Rect {
        x: ORIGIN.0 + 11.0 * (SLOT + GAP) + 24.0,
        y: ORIGIN.1,
        w: SHOP_COLS as f32 * SHOP_CELL_W + 16.0,
        h: rows as f32 * SHOP_CELL_H + 58.0,
    }
}

fn draw_shop(
    shop: &mut Shop,
    inv: &[Option<InvSlot>],
    mouse: (f32, f32),
    now: f64,
    click_spent: bool,
    out: &mut Vec<ClientMessage>,
) {
    let r = shop_window_rect(shop.items.len());
    draw_rectangle(r.x, r.y, r.w, r.h, PANEL_BG);
    draw_rectangle_lines(r.x, r.y, r.w, r.h, 2.0, PANEL_BORDER);
    shadow_text("Shop", r.x + 8.0, r.y + 20.0, 22.0, TEXT);
    let wallet = coin_total(inv);

    for (i, entry) in shop.items.iter().enumerate() {
        let (col, row) = (i % SHOP_COLS, i / SHOP_COLS);
        let cx = r.x + 8.0 + col as f32 * SHOP_CELL_W;
        let cy = r.y + 28.0 + row as f32 * SHOP_CELL_H;
        let cell = Rect {
            x: cx,
            y: cy,
            w: SHOP_CELL_W - 6.0,
            h: SHOP_CELL_H - 4.0,
        };
        let hover = cell.contains(mouse);
        if hover {
            draw_rectangle(
                cell.x,
                cell.y,
                cell.w,
                cell.h,
                Color::new(1.0, 1.0, 1.0, 0.07),
            );
        }
        let flashing = shop.flash.is_some_and(|(it, _)| it == entry.item);
        let affordable = wallet >= entry.price as u64;
        // Glyph (greyed until SlotChanged while a buy is in flight).
        draw_item(
            cx + 4.0,
            cy + 4.0,
            entry.item,
            1,
            if flashing { 0.4 } else { 1.0 },
        );
        let name = entry.item.data().name;
        draw_text(
            name,
            cx + SLOT + 2.0,
            cy + 16.0,
            16.0,
            if affordable { TEXT } else { DIM },
        );
        let price = format_coins(entry.price as u64);
        draw_text(
            &price,
            cx + SLOT + 2.0,
            cy + 34.0,
            16.0,
            if affordable { PRICE_COLOR } else { CANT_AFFORD },
        );
        // `!click_spent`: a carried stack dropped on this window sold it —
        // the same press must not also buy the cell under the cursor.
        if hover && is_mouse_button_pressed(MouseButton::Left) && !flashing && !click_spent {
            // Server-authoritative: optimistic grey only.
            out.push(ClientMessage::BuyItem {
                npc_id: shop.npc_id,
                item: entry.item,
                count: 1,
            });
            shop.flash = Some((entry.item, now));
        }
        if flashing {
            draw_rectangle(
                cell.x,
                cell.y,
                cell.w,
                cell.h,
                Color::new(0.6, 0.6, 0.6, 0.25),
            );
        }
    }
    draw_text(
        "Drag or shift-click items here to sell (20% of value)",
        r.x + 8.0,
        r.y + r.h - 10.0,
        15.0,
        DIM,
    );
}

fn draw_button(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    label: &str,
    enabled: bool,
    mouse: (f32, f32),
) -> bool {
    let r = Rect { x, y, w, h };
    let hover = enabled && r.contains(mouse);
    let bg = if !enabled {
        Color::new(0.12, 0.13, 0.18, 1.0)
    } else if hover {
        BUTTON_BG_HOVER
    } else {
        BUTTON_BG
    };
    draw_rectangle(x, y, w, h, bg);
    draw_rectangle_lines(x, y, w, h, 1.5, PANEL_BORDER);
    let dims = measure_text(label, None, 18, 1.0);
    draw_text(
        label,
        x + (w - dims.width) * 0.5,
        y + h * 0.5 + dims.height * 0.4,
        18.0,
        if enabled { TEXT } else { DIM },
    );
    hover && is_mouse_button_pressed(MouseButton::Left)
}

/// Greedy word wrap by measured width.
fn wrap_text(text: &str, max_w: f32, font: u16) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if measure_text(&candidate, None, font, 1.0).width > max_w && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}
