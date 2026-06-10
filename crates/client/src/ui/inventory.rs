//! Inventory screen: hotbar (always visible), backpack grid, armor +
//! accessory column with silhouettes, trash slot, drag-and-drop with a
//! mouse-carried stack, RMB half-split, shift-click quick-move, tooltips
//! from `ITEM_DATA`, the coin readout, and the chest panel.
//!
//! The UI never mutates the inventory mirror directly: every action becomes
//! a `ClientMessage` and the server's `SlotChanged`/`ChestSlotChanged`
//! deltas update the mirror — optimism is limited to visuals (the carried
//! stack dims its source slot), so the client can never drift from the
//! authority.

use macroquad::prelude::*;

use ferraria_shared::inventory_ops::{inventory_role, quick_move_dest, SlotRole};
use ferraria_shared::items::{
    inventory as layout, set_bonus, AccessoryEffect, ArmorSlot, Consumable, InvSlot, ItemId,
    SetBonus, WeaponKind,
};
use ferraria_shared::protocol::ClientMessage;
use ferraria_shared::tiles::ToolKind;
use ferraria_shared::world::CHEST_SLOTS;
use ferraria_shared::{COPPER_PER_GOLD, COPPER_PER_PLATINUM, COPPER_PER_SILVER};

use super::shadow_text;

// ---- Layout ---------------------------------------------------------------------

pub const SLOT: f32 = 40.0;
pub const GAP: f32 = 4.0;
const STEP: f32 = SLOT + GAP;
/// Top-left of the hotbar row (everything else hangs off it).
pub const ORIGIN: (f32, f32) = (12.0, 58.0);
const COLS: usize = 10;

const SLOT_BG: Color = Color::new(0.08, 0.10, 0.16, 0.85);
const SLOT_BG_EQUIP: Color = Color::new(0.10, 0.09, 0.16, 0.85);
const SLOT_BORDER: Color = Color::new(1.0, 1.0, 1.0, 0.18);
const SELECT_RING: Color = Color::new(1.0, 0.85, 0.30, 0.95);
const TEXT: Color = Color::new(0.92, 0.92, 0.95, 1.0);
const DIM: Color = Color::new(0.62, 0.62, 0.70, 1.0);
const TIP_BG: Color = Color::new(0.04, 0.05, 0.09, 0.93);
const SILHOUETTE: Color = Color::new(1.0, 1.0, 1.0, 0.16);

/// Where one on-screen slot lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAddr {
    Inv(usize),
    Chest(usize),
}

/// What the mouse is carrying (visually — the items stay in their slot
/// server-side until the drop op round-trips).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carry {
    All,
    Half,
}

#[derive(Debug, Clone, Copy)]
struct Carried {
    from: UiAddr,
    amount: Carry,
}

/// Mirror of the chest the player has open.
pub struct ChestMirror {
    pub origin: (u32, u32),
    pub slots: Vec<Option<InvSlot>>,
}

impl ChestMirror {
    pub fn new(origin: (u32, u32), slots: Vec<Option<InvSlot>>) -> ChestMirror {
        let mut slots = slots;
        slots.resize(CHEST_SLOTS, None);
        ChestMirror { origin, slots }
    }
}

pub struct InventoryUi {
    pub open: bool,
    carried: Option<Carried>,
    /// Cached coin readout (reformatted only when the total changes).
    coin_text: String,
    coin_key: u64,
}

impl InventoryUi {
    pub fn new() -> InventoryUi {
        InventoryUi {
            open: false,
            carried: None,
            coin_text: String::new(),
            coin_key: u64::MAX,
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.carried = None;
    }

    /// Draws the hotbar (always) and, when open, the full inventory screen
    /// (+ chest panel if a chest is open). Mouse actions append protocol
    /// messages to `out`.
    pub fn frame(
        &mut self,
        inv: &[Option<InvSlot>],
        chest: Option<&ChestMirror>,
        selected: u8,
        out: &mut Vec<ClientMessage>,
    ) {
        let mouse = mouse_position();
        let mut hovered: Option<UiAddr> = None;

        // ---- Hotbar row (always visible). --------------------------------
        for i in 0..layout::HOTBAR {
            let r = slot_rect(UiAddr::Inv(i));
            self.draw_slot(r, UiAddr::Inv(i), inv.get(i).copied().flatten());
            if i as u8 == selected {
                draw_rectangle_lines(r.x - 2.0, r.y - 2.0, r.w + 4.0, r.h + 4.0, 3.0, SELECT_RING);
            }
            shadow_text(digit_label(i), r.x + 3.0, r.y + 12.0, 14.0, DIM);
            if r.contains(mouse) {
                hovered = Some(UiAddr::Inv(i));
            }
        }
        if !self.open {
            // Tooltip for hotbar hover even with the screen closed.
            if let Some(addr) = hovered {
                if let Some(stack) = slot_at(inv, chest, addr) {
                    draw_tooltip(mouse, stack);
                }
            }
            self.carried = None;
            return;
        }

        // ---- Backpack grid. ------------------------------------------------
        for i in layout::BACKPACK_START..layout::ARMOR_START {
            let r = slot_rect(UiAddr::Inv(i));
            self.draw_slot(r, UiAddr::Inv(i), inv.get(i).copied().flatten());
            if r.contains(mouse) {
                hovered = Some(UiAddr::Inv(i));
            }
        }
        // ---- Armor + accessory column + trash. ------------------------------
        for i in layout::ARMOR_START..layout::TOTAL {
            let r = slot_rect(UiAddr::Inv(i));
            self.draw_slot(r, UiAddr::Inv(i), inv.get(i).copied().flatten());
            if r.contains(mouse) {
                hovered = Some(UiAddr::Inv(i));
            }
        }

        // ---- Chest panel. -----------------------------------------------------
        if let Some(c) = chest {
            let title = slot_rect(UiAddr::Chest(0));
            shadow_text("Chest", title.x, title.y - 8.0, 20.0, TEXT);
            for i in 0..CHEST_SLOTS {
                let r = slot_rect(UiAddr::Chest(i));
                self.draw_slot(r, UiAddr::Chest(i), c.slots.get(i).copied().flatten());
                if r.contains(mouse) {
                    hovered = Some(UiAddr::Chest(i));
                }
            }
        }

        // ---- Coin readout. ------------------------------------------------------
        let coins = coin_total(inv);
        if coins != self.coin_key {
            self.coin_key = coins;
            self.coin_text = format!("Coins: {}", format_coins(coins));
        }
        let grid_bottom = ORIGIN.1 + 5.0 * STEP;
        shadow_text(&self.coin_text, ORIGIN.0, grid_bottom + 14.0, 20.0, TEXT);

        // ---- Mouse actions. --------------------------------------------------------
        self.handle_clicks(inv, chest, hovered, out);

        // ---- Carried stack + tooltip, over everything. -------------------------------
        if let Some(c) = self.carried {
            if let Some(stack) = slot_at(inv, chest, c.from) {
                let count = match c.amount {
                    Carry::All => stack.count,
                    Carry::Half => stack.count.div_ceil(2),
                };
                draw_item(
                    mouse.0 - SLOT * 0.3,
                    mouse.1 - SLOT * 0.3,
                    stack.item,
                    count,
                    1.0,
                );
            } else {
                self.carried = None; // source emptied under us (server delta)
            }
        } else if let Some(addr) = hovered {
            if let Some(stack) = slot_at(inv, chest, addr) {
                draw_tooltip(mouse, stack);
            }
        }
    }

    /// LMB/RMB/shift-click/Q handling against the hovered slot.
    fn handle_clicks(
        &mut self,
        inv: &[Option<InvSlot>],
        chest: Option<&ChestMirror>,
        hovered: Option<UiAddr>,
        out: &mut Vec<ClientMessage>,
    ) {
        let shift = is_key_down(KeyCode::LeftShift) || is_key_down(KeyCode::RightShift);
        let lmb = is_mouse_button_pressed(MouseButton::Left);
        let rmb = is_mouse_button_pressed(MouseButton::Right);
        if !lmb && !rmb && !is_key_pressed(KeyCode::Q) {
            return;
        }

        let Some(addr) = hovered else {
            // Click outside any slot: the carried stack goes back (nothing
            // ever left the source slot server-side).
            if lmb || rmb {
                self.carried = None;
            }
            return;
        };
        let stack = slot_at(inv, chest, addr);

        // Q: drop the hovered stack on the ground.
        if is_key_pressed(KeyCode::Q) && self.carried.is_none() {
            if let (UiAddr::Inv(i), Some(s)) = (addr, stack) {
                out.push(ClientMessage::DropItem {
                    slot: i as u8,
                    count: s.count,
                });
            }
            return;
        }

        if let Some(c) = self.carried {
            // Drop the carried stack here.
            if lmb || rmb {
                if c.from != addr {
                    push_slot_op(c, addr, out);
                }
                self.carried = None;
            }
            return;
        }

        let Some(s) = stack else { return };
        if shift && lmb {
            // Quick-move to the paired region.
            if let Some(msg) = quick_move(inv, chest, addr, s) {
                out.push(msg);
            }
        } else if lmb {
            self.carried = Some(Carried {
                from: addr,
                amount: Carry::All,
            });
        } else if rmb {
            // Half-pickup is an inventory-only wire op; chest slots carry
            // whole stacks on RMB too.
            let amount = match addr {
                UiAddr::Inv(_) if s.count > 1 => Carry::Half,
                _ => Carry::All,
            };
            self.carried = Some(Carried { from: addr, amount });
        }
    }

    fn draw_slot(&self, r: Rect, addr: UiAddr, stack: Option<InvSlot>) {
        let role = match addr {
            UiAddr::Inv(i) => inventory_role(i).unwrap_or(SlotRole::Plain),
            UiAddr::Chest(_) => SlotRole::Plain,
        };
        let bg = if matches!(role, SlotRole::Plain) {
            SLOT_BG
        } else {
            SLOT_BG_EQUIP
        };
        draw_rectangle(r.x, r.y, r.w, r.h, bg);
        draw_rectangle_lines(r.x, r.y, r.w, r.h, 2.0, SLOT_BORDER);
        match stack {
            Some(s) => {
                // Dim the source of the carried stack instead of hiding it:
                // the move hasn't happened server-side yet.
                let dim = if self.carried.is_some_and(|c| c.from == addr) {
                    0.35
                } else {
                    1.0
                };
                draw_item(r.x + 6.0, r.y + 6.0, s.item, s.count, dim);
            }
            None => draw_silhouette(r, role),
        }
    }
}

impl Default for InventoryUi {
    fn default() -> Self {
        InventoryUi::new()
    }
}

// ---- Geometry ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, (mx, my): (f32, f32)) -> bool {
        mx >= self.x && mx <= self.x + self.w && my >= self.y && my <= self.y + self.h
    }
}

/// Screen rectangle of a slot. Inventory: hotbar row 0, backpack rows 1–4,
/// then the equipment column to the right of the grid (armor, accessories,
/// trash top-to-bottom). Chest: a 10×4 grid right of the equipment column.
pub fn slot_rect(addr: UiAddr) -> Rect {
    let (x, y) = match addr {
        UiAddr::Inv(i) if i < layout::ARMOR_START => {
            let (col, row) = (i % COLS, i / COLS);
            (ORIGIN.0 + col as f32 * STEP, ORIGIN.1 + row as f32 * STEP)
        }
        UiAddr::Inv(i) => {
            // Equipment column: armor 0–2, accessories 3–5, trash 6 (with
            // small gaps between the groups).
            let n = i - layout::ARMOR_START;
            let group_gap = match n {
                0..=2 => 0.0,
                3..=5 => 8.0,
                _ => 16.0,
            };
            (
                ORIGIN.0 + COLS as f32 * STEP + 12.0,
                ORIGIN.1 + n as f32 * STEP + group_gap,
            )
        }
        UiAddr::Chest(i) => {
            let (col, row) = (i % COLS, i / COLS);
            (
                ORIGIN.0 + (COLS + 1) as f32 * STEP + 24.0 + col as f32 * STEP,
                ORIGIN.1 + STEP + row as f32 * STEP, // below its title
            )
        }
    };
    Rect {
        x,
        y,
        w: SLOT,
        h: SLOT,
    }
}

fn slot_at(inv: &[Option<InvSlot>], chest: Option<&ChestMirror>, addr: UiAddr) -> Option<InvSlot> {
    match addr {
        UiAddr::Inv(i) => inv.get(i).copied().flatten(),
        UiAddr::Chest(i) => chest.and_then(|c| c.slots.get(i).copied().flatten()),
    }
}

const DIGIT_LABELS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];

fn digit_label(i: usize) -> &'static str {
    DIGIT_LABELS.get(i).copied().unwrap_or("")
}

// ---- Ops --------------------------------------------------------------------------

/// Maps a carried-stack drop onto the wire op.
fn push_slot_op(c: Carried, to: UiAddr, out: &mut Vec<ClientMessage>) {
    match (c.from, to) {
        (UiAddr::Inv(f), UiAddr::Inv(t)) => out.push(match c.amount {
            Carry::All => ClientMessage::MoveSlot {
                from: f as u8,
                to: t as u8,
            },
            Carry::Half => ClientMessage::SplitSlot {
                from: f as u8,
                to: t as u8,
            },
        }),
        // Chest moves have no split variant on the wire; a Half carry
        // dropped on/from a chest cancels (nothing was taken yet).
        (UiAddr::Inv(f), UiAddr::Chest(t)) if c.amount == Carry::All => {
            out.push(ClientMessage::ChestMoveSlot {
                chest_slot: t as u8,
                inv_slot: f as u8,
                to_chest: true,
            })
        }
        (UiAddr::Chest(f), UiAddr::Inv(t)) if c.amount == Carry::All => {
            out.push(ClientMessage::ChestMoveSlot {
                chest_slot: f as u8,
                inv_slot: t as u8,
                to_chest: false,
            })
        }
        _ => {}
    }
}

/// Shift-click destination: hotbar ↔ backpack, or inventory ↔ chest while
/// one is open.
fn quick_move(
    inv: &[Option<InvSlot>],
    chest: Option<&ChestMirror>,
    addr: UiAddr,
    s: InvSlot,
) -> Option<ClientMessage> {
    match (addr, chest) {
        (UiAddr::Inv(i), Some(c)) if i < layout::ARMOR_START => {
            let dest = quick_move_dest(&c.slots, s.item, 0..CHEST_SLOTS)?;
            Some(ClientMessage::ChestMoveSlot {
                chest_slot: dest as u8,
                inv_slot: i as u8,
                to_chest: true,
            })
        }
        (UiAddr::Chest(i), _) => {
            let dest = quick_move_dest(inv, s.item, layout::CRAFTING_SLOTS)?;
            Some(ClientMessage::ChestMoveSlot {
                chest_slot: i as u8,
                inv_slot: dest as u8,
                to_chest: false,
            })
        }
        (UiAddr::Inv(i), None) if i < layout::ARMOR_START => {
            // Hotbar -> backpack and vice versa.
            let range = if i < layout::HOTBAR {
                layout::BACKPACK_START..layout::ARMOR_START
            } else {
                0..layout::HOTBAR
            };
            let dest = quick_move_dest(inv, s.item, range)?;
            Some(ClientMessage::MoveSlot {
                from: i as u8,
                to: dest as u8,
            })
        }
        // Equipment/trash: quick-move back into the main inventory.
        (UiAddr::Inv(i), _) => {
            let dest = quick_move_dest(inv, s.item, layout::CRAFTING_SLOTS)?;
            Some(ClientMessage::MoveSlot {
                from: i as u8,
                to: dest as u8,
            })
        }
    }
}

// ---- Item rendering ------------------------------------------------------------------

/// Item glyph: colored square + 2-letter code (real sprites come with the
/// texture-atlas PR) and a count badge.
pub fn draw_item(x: f32, y: f32, item: ItemId, count: u16, dim: f32) {
    let size = SLOT - 12.0;
    let c = crate::render::item_color(item);
    draw_rectangle(
        x,
        y,
        size,
        size,
        Color::new(c.r * dim, c.g * dim, c.b * dim, 1.0),
    );
    let code = glyph_code(item);
    draw_text(
        code.as_str(),
        x + 4.0,
        y + size - 8.0,
        18.0,
        Color::new(0.0, 0.0, 0.0, 0.75 * dim),
    );
    if count > 1 {
        let badge = count_buf(count);
        shadow_text(
            badge.as_str(),
            x + 2.0,
            y + size + 5.0,
            16.0,
            Color::new(TEXT.r, TEXT.g, TEXT.b, dim),
        );
    }
}

/// Stack-allocated 2-letter item code: initials of the first two words,
/// else the first two letters.
fn glyph_code(item: ItemId) -> heapless_str::Str<2> {
    let name = item.data().name;
    let mut words = name.split_whitespace();
    let first = words.next().unwrap_or("?");
    let mut out = heapless_str::Str::new();
    match words.next() {
        Some(second) => {
            out.push(first.chars().next().unwrap_or('?'));
            out.push(second.chars().next().unwrap_or('?'));
        }
        None => {
            let mut chars = first.chars();
            out.push(chars.next().unwrap_or('?'));
            out.push(chars.next().unwrap_or(' '));
        }
    }
    out
}

/// Stack-allocated count text (avoids per-frame heap formatting).
fn count_buf(count: u16) -> heapless_str::Str<5> {
    let mut out = heapless_str::Str::new();
    let mut n = count;
    let mut digits = [0u8; 5];
    let mut len = 0;
    loop {
        digits[len] = (n % 10) as u8;
        len += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for d in digits[..len].iter().rev() {
        out.push((b'0' + d) as char);
    }
    out
}

/// Tiny fixed-capacity string so glyphs/counts don't allocate per frame.
mod heapless_str {
    pub struct Str<const N: usize> {
        buf: [u8; N],
        len: usize,
    }

    impl<const N: usize> Str<N> {
        pub fn new() -> Self {
            Str {
                buf: [0; N],
                len: 0,
            }
        }

        /// Pushes an ASCII char; non-ASCII becomes '?', overflow is dropped.
        pub fn push(&mut self, c: char) {
            if self.len < N {
                self.buf[self.len] = if c.is_ascii() { c as u8 } else { b'?' };
                self.len += 1;
            }
        }

        pub fn as_str(&self) -> &str {
            // ASCII by construction.
            std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
        }
    }
}

/// Faint glyph hinting what an empty equipment slot takes.
fn draw_silhouette(r: Rect, role: SlotRole) {
    let cx = r.x + r.w / 2.0;
    let cy = r.y + r.h / 2.0;
    match role {
        SlotRole::Armor(ArmorSlot::Head) => {
            draw_circle(cx, cy - 2.0, 9.0, SILHOUETTE);
            draw_rectangle(cx - 9.0, cy + 2.0, 18.0, 5.0, SILHOUETTE);
        }
        SlotRole::Armor(ArmorSlot::Chest) => {
            draw_rectangle(cx - 9.0, cy - 9.0, 18.0, 18.0, SILHOUETTE);
            draw_rectangle(cx - 13.0, cy - 9.0, 4.0, 10.0, SILHOUETTE);
            draw_rectangle(cx + 9.0, cy - 9.0, 4.0, 10.0, SILHOUETTE);
        }
        SlotRole::Armor(ArmorSlot::Legs) => {
            draw_rectangle(cx - 8.0, cy - 9.0, 6.0, 18.0, SILHOUETTE);
            draw_rectangle(cx + 2.0, cy - 9.0, 6.0, 18.0, SILHOUETTE);
        }
        SlotRole::Accessory => {
            draw_circle_lines(cx, cy, 8.0, 2.5, SILHOUETTE);
        }
        SlotRole::Trash => {
            draw_line(cx - 7.0, cy - 7.0, cx + 7.0, cy + 7.0, 2.5, SILHOUETTE);
            draw_line(cx + 7.0, cy - 7.0, cx - 7.0, cy + 7.0, 2.5, SILHOUETTE);
        }
        SlotRole::Plain => {}
    }
}

// ---- Tooltips & coins -----------------------------------------------------------------

/// Item tooltip at the mouse: name, combat/tool/armor/accessory/consumable
/// lines, and the coin value. Allocates only while actually hovering.
pub fn draw_tooltip(mouse: (f32, f32), stack: InvSlot) {
    let d = stack.item.data();
    let mut lines: Vec<(String, Color)> = vec![(d.name.to_string(), TEXT)];
    if let Some(w) = d.weapon {
        let kind = match w.kind {
            WeaponKind::Melee => "melee",
            WeaponKind::Bow => "bow",
            WeaponKind::Arrow => "arrow",
        };
        if w.use_secs > 0.0 {
            lines.push((
                format!("{} damage ({kind}, {:.2}s use)", w.damage, w.use_secs),
                DIM,
            ));
        } else {
            lines.push((format!("{} damage ({kind})", w.damage), DIM));
        }
    }
    if let Some(t) = d.tool {
        let kind = match t.kind {
            ToolKind::Pick => "pick",
            ToolKind::Axe => "axe",
            ToolKind::Hammer => "hammer",
            ToolKind::Any | ToolKind::None => "tool",
        };
        lines.push((
            format!("{}% {kind} power ({:.2}s use)", t.power, t.use_secs),
            DIM,
        ));
    }
    if let Some(a) = d.armor {
        lines.push((format!("{} defense", a.defense), DIM));
        if let Some(set) = a.set {
            let bonus = match set_bonus(set) {
                SetBonus::Defense(n) => format!("Set bonus: +{n} defense"),
                SetBonus::EmberFury => {
                    "Set bonus: +10% melee damage, immune to Burning".to_string()
                }
            };
            lines.push((bonus, DIM));
        }
    }
    if let Some(e) = d.accessory {
        lines.push((accessory_text(e).to_string(), DIM));
    }
    if let Some(c) = d.consumable {
        let text = match c {
            Consumable::Heal(hp) => format!("Restores {hp} HP"),
            Consumable::MaxHpUp(hp) => format!("+{hp} max HP (permanent)"),
            Consumable::SummonBoss(_) => "Summons a boss".to_string(),
            Consumable::TeleportToSpawn => "Teleports you home".to_string(),
        };
        lines.push((text, DIM));
    }
    if d.places.is_some() {
        lines.push(("Can be placed".to_string(), DIM));
    }
    if d.value > 0 {
        lines.push((format!("Value: {}", format_coins(d.value as u64)), DIM));
    }

    let width = lines
        .iter()
        .map(|(l, _)| measure_text(l, None, 18, 1.0).width)
        .fold(80.0, f32::max)
        + 16.0;
    let height = lines.len() as f32 * 20.0 + 10.0;
    let x = (mouse.0 + 16.0).min(screen_width() - width - 4.0);
    let y = (mouse.1 + 16.0).min(screen_height() - height - 4.0);
    draw_rectangle(x, y, width, height, TIP_BG);
    draw_rectangle_lines(x, y, width, height, 1.5, SLOT_BORDER);
    for (i, (line, color)) in lines.iter().enumerate() {
        draw_text(line, x + 8.0, y + 20.0 + i as f32 * 20.0, 18.0, *color);
    }
}

fn accessory_text(e: AccessoryEffect) -> &'static str {
    match e {
        AccessoryEffect::RunSpeed => "+25% max run speed",
        AccessoryEffect::DoubleJump => "Grants a double jump",
        AccessoryEffect::NoFallDamage => "Negates fall damage",
        AccessoryEffect::HpRegen => "+0.5 HP/s passive regen",
        AccessoryEffect::FireWard => "Immune to Burning; lava deals half damage",
        AccessoryEffect::SlimeFriend => "Slimes never aggro",
        AccessoryEffect::DamageBoost => "+10% all damage",
        AccessoryEffect::DefenseBoost => "+4 defense",
    }
}

/// Total carried coin value (hotbar + backpack), in copper.
pub fn coin_total(inv: &[Option<InvSlot>]) -> u64 {
    inv.iter()
        .take(layout::ARMOR_START)
        .flatten()
        .filter(|s| {
            matches!(
                s.item,
                ItemId::CopperCoin | ItemId::SilverCoin | ItemId::GoldCoin | ItemId::PlatinumCoin
            )
        })
        .map(|s| s.item.data().value as u64 * s.count as u64)
        .sum()
}

/// "1p 23g 45s 67c", skipping zero denominations ("0c" when empty).
pub fn format_coins(copper: u64) -> String {
    if copper == 0 {
        return "0c".to_string();
    }
    let p = copper / COPPER_PER_PLATINUM as u64;
    let g = copper % COPPER_PER_PLATINUM as u64 / COPPER_PER_GOLD as u64;
    let s = copper % COPPER_PER_GOLD as u64 / COPPER_PER_SILVER as u64;
    let c = copper % COPPER_PER_SILVER as u64;
    let mut out = String::new();
    for (n, unit) in [(p, "p"), (g, "g"), (s, "s"), (c, "c")] {
        if n > 0 {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&n.to_string());
            out.push_str(unit);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coin_formatting() {
        assert_eq!(format_coins(0), "0c");
        assert_eq!(format_coins(99), "99c");
        assert_eq!(format_coins(100), "1s");
        assert_eq!(format_coins(10_203), "1g 2s 3c");
        assert_eq!(format_coins(1_000_000), "1p");
        assert_eq!(format_coins(1_234_567), "1p 23g 45s 67c");
    }

    #[test]
    fn coin_total_counts_carried_coins_only() {
        let mut inv = vec![None; layout::TOTAL];
        inv[0] = Some(InvSlot::new(ItemId::GoldCoin, 2));
        inv[11] = Some(InvSlot::new(ItemId::CopperCoin, 34));
        inv[12] = Some(InvSlot::new(ItemId::GoldBar, 5)); // not a coin
        inv[layout::TRASH] = Some(InvSlot::new(ItemId::PlatinumCoin, 1)); // trash doesn't count
        assert_eq!(coin_total(&inv), 20_034);
    }

    #[test]
    fn glyph_codes() {
        assert_eq!(glyph_code(ItemId::Wood).as_str(), "Wo");
        assert_eq!(glyph_code(ItemId::WoodPickaxe).as_str(), "WP");
        assert_eq!(glyph_code(ItemId::Gel).as_str(), "Ge");
        assert_eq!(glyph_code(ItemId::LesserHealingPotion).as_str(), "LH");
    }

    #[test]
    fn count_buffer() {
        assert_eq!(count_buf(1).as_str(), "1");
        assert_eq!(count_buf(999).as_str(), "999");
        assert_eq!(count_buf(65535).as_str(), "65535");
    }
}
