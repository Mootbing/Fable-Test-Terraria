//! The hotbar strip: 1–0 / mouse-wheel slot selection and a minimal
//! top-center render (swatch + count + selected highlight).
//!
//! Kept out of `ui.rs` on purpose: the full inventory/crafting UI lands in a
//! parallel branch, and this module owning only the hotbar keeps the merge
//! surface small.

use macroquad::prelude::*;

use ferraria_shared::items::{inventory, InvSlot};

use crate::render::item_color;
use crate::ui::shadow_text;

const SLOT: f32 = 38.0;
const PAD: f32 = 4.0;
const BG: Color = Color::new(0.0, 0.0, 0.0, 0.45);
const BORDER: Color = Color::new(1.0, 1.0, 1.0, 0.25);
const SELECTED: Color = Color::new(1.0, 0.9, 0.3, 0.95);

/// Applies this frame's hotbar selection input (1–0 keys, mouse wheel).
/// Returns `true` when the selection changed (caller sends `SelectSlot`).
pub fn selection_input(selected: &mut u8) -> bool {
    const KEYS: [KeyCode; 10] = [
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
    let before = *selected;
    for (i, key) in KEYS.iter().enumerate() {
        if is_key_pressed(*key) {
            *selected = i as u8;
        }
    }
    let wheel = mouse_wheel().1;
    let n = inventory::HOTBAR as u8;
    if wheel > 0.0 {
        *selected = (*selected + n - 1) % n;
    } else if wheel < 0.0 {
        *selected = (*selected + 1) % n;
    }
    *selected != before
}

/// Draws the hotbar (first 10 inventory slots) top-center, highlighting
/// `selected` and naming the held item under it.
pub fn draw(slots: &[Option<InvSlot>], selected: u8) {
    // Slot key labels: 1..9, then 0.
    const LABELS: [&str; inventory::HOTBAR] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
    let total_w = inventory::HOTBAR as f32 * (SLOT + PAD) - PAD;
    let x0 = (screen_width() - total_w) * 0.5;
    let y0 = 8.0;
    for (i, label) in LABELS.iter().enumerate() {
        let x = x0 + i as f32 * (SLOT + PAD);
        draw_rectangle(x, y0, SLOT, SLOT, BG);
        let border = if i as u8 == selected {
            SELECTED
        } else {
            BORDER
        };
        let thick = if i as u8 == selected { 3.0 } else { 1.5 };
        draw_rectangle_lines(x, y0, SLOT, SLOT, thick, border);
        shadow_text(
            label,
            x + 3.0,
            y0 + 12.0,
            14.0,
            Color::new(0.8, 0.8, 0.85, 0.9),
        );
        if let Some(stack) = slots.get(i).copied().flatten() {
            draw_rectangle(
                x + SLOT * 0.3,
                y0 + SLOT * 0.3,
                SLOT * 0.4,
                SLOT * 0.4,
                item_color(stack.item),
            );
            if stack.count > 1 {
                shadow_text(
                    &stack.count.to_string(),
                    x + 4.0,
                    y0 + SLOT - 4.0,
                    16.0,
                    WHITE,
                );
            }
        }
    }
    // Held item name under the selected slot.
    if let Some(stack) = slots.get(selected as usize).copied().flatten() {
        let x = x0 + selected as f32 * (SLOT + PAD);
        shadow_text(
            stack.item.data().name,
            x,
            y0 + SLOT + 16.0,
            16.0,
            Color::new(0.92, 0.92, 0.95, 0.95),
        );
    }
}
