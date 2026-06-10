//! Crafting panel inside the inventory screen (DESIGN §4.4): recipes from
//! `shared::crafting` filtered by nearby stations (mirrored from world tiles
//! via `crafting::stations_in_range` — the server stays the authority),
//! green/grey by availability, click-to-craft with an optimistic grey-flash
//! reconciled by `SlotChanged`, ingredient tooltips with have/need counts,
//! a scrollable list, and station tabs.

use macroquad::prelude::*;

use ferraria_shared::crafting::{can_craft, Recipe, Station, StationSet, RECIPES};
use ferraria_shared::items::{InvSlot, ItemId};
use ferraria_shared::protocol::ClientMessage;

use super::inventory::{Rect, GAP, ORIGIN, SLOT};

const ROW_H: f32 = 26.0;
const VISIBLE_ROWS: usize = 9;
const PANEL_BG: Color = Color::new(0.0, 0.0, 0.0, 0.45);
const TEXT: Color = Color::new(0.92, 0.92, 0.95, 1.0);
const DIM: Color = Color::new(0.55, 0.55, 0.62, 1.0);
const OK: Color = Color::new(0.45, 0.95, 0.45, 1.0);
const NO: Color = Color::new(0.60, 0.60, 0.66, 0.85);
const TAB_ON: Color = Color::new(0.28, 0.34, 0.48, 1.0);
const TAB_OFF: Color = Color::new(0.12, 0.14, 0.22, 0.9);
const FLASH_SECS: f64 = 0.25;

/// Station filter tabs: All + one per station.
const TABS: [(Option<Station>, &str); 8] = [
    (None, "All"),
    (Some(Station::Hands), "Hands"),
    (Some(Station::Workbench), "Bench"),
    (Some(Station::Furnace), "Furnace"),
    (Some(Station::Anvil), "Anvil"),
    (Some(Station::InfernalForge), "Forge"),
    (Some(Station::RitualAltar), "Altar"),
    (Some(Station::Bottle), "Bottle"),
];

pub struct CraftingUi {
    tab: Option<Station>,
    scroll: usize,
    /// Optimistic click feedback: (recipe id, started-at) — the row flashes
    /// grey until the `SlotChanged` reconcile lands (or it times out).
    flash: Option<(u16, f64)>,
}

impl CraftingUi {
    pub fn new() -> CraftingUi {
        CraftingUi {
            tab: None,
            scroll: 0,
            flash: None,
        }
    }

    /// Draws the panel below the inventory grid. `stations` mirrors the
    /// tiles near the player; clicks append `Craft` messages to `out`.
    pub fn frame(
        &mut self,
        inv: &[Option<InvSlot>],
        stations: StationSet,
        now: f64,
        out: &mut Vec<ClientMessage>,
    ) {
        let x0 = ORIGIN.0;
        let y0 = ORIGIN.1 + 5.0 * (SLOT + GAP) + 28.0;
        let width = 10.0 * (SLOT + GAP) - GAP;
        let mouse = mouse_position();

        // ---- Station tabs. ---------------------------------------------------
        let mut tx = x0;
        for (station, label) in TABS {
            let w = measure_text(label, None, 18, 1.0).width + 14.0;
            let r = Rect {
                x: tx,
                y: y0,
                w,
                h: 22.0,
            };
            let active = self.tab == station;
            let in_range = station.is_none_or(|s| stations.contains(s));
            draw_rectangle(r.x, r.y, r.w, r.h, if active { TAB_ON } else { TAB_OFF });
            draw_text(
                label,
                r.x + 7.0,
                r.y + 16.0,
                18.0,
                if in_range { TEXT } else { DIM },
            );
            if r.contains(mouse) && is_mouse_button_pressed(MouseButton::Left) {
                self.tab = station;
                self.scroll = 0;
            }
            tx += w + 6.0;
        }

        // ---- Recipe list. ------------------------------------------------------
        let list_y = y0 + 28.0;
        let list_h = VISIBLE_ROWS as f32 * ROW_H + 8.0;
        draw_rectangle(x0, list_y, width, list_h, PANEL_BG);
        let panel = Rect {
            x: x0,
            y: list_y,
            w: width,
            h: list_h,
        };

        // "All" shows what the nearby stations can make; a station tab
        // browses that station's full book (greyed while out of range).
        let rows: Vec<&'static Recipe> = RECIPES
            .iter()
            .filter(|r| match self.tab {
                None => stations.contains(r.station),
                Some(tab) => r.station == tab,
            })
            .collect();

        // Wheel scrolls while hovering the panel.
        if panel.contains(mouse) {
            let (_, wheel) = mouse_wheel();
            if wheel < 0.0 {
                self.scroll = (self.scroll + 1).min(rows.len().saturating_sub(VISIBLE_ROWS));
            } else if wheel > 0.0 {
                self.scroll = self.scroll.saturating_sub(1);
            }
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(VISIBLE_ROWS));
        if self.flash.is_some_and(|(_, at)| now - at > FLASH_SECS) {
            self.flash = None;
        }

        let mut hovered: Option<&'static Recipe> = None;
        for (row, recipe) in rows.iter().skip(self.scroll).take(VISIBLE_ROWS).enumerate() {
            let ry = list_y + 4.0 + row as f32 * ROW_H;
            let r = Rect {
                x: x0 + 4.0,
                y: ry,
                w: width - 8.0,
                h: ROW_H - 2.0,
            };
            let craftable = stations.contains(recipe.station) && can_craft(recipe, inv);
            let flashing = self.flash.is_some_and(|(id, _)| id == recipe.id);
            if r.contains(mouse) {
                hovered = Some(recipe);
                draw_rectangle(r.x, r.y, r.w, r.h, Color::new(1.0, 1.0, 1.0, 0.07));
            }
            if flashing {
                draw_rectangle(r.x, r.y, r.w, r.h, Color::new(0.6, 0.6, 0.6, 0.35));
            }
            // Mini glyph + label.
            let c = crate::render::item_color(recipe.output);
            let dim = if craftable { 1.0 } else { 0.45 };
            draw_rectangle(
                r.x + 2.0,
                r.y + 4.0,
                16.0,
                16.0,
                Color::new(c.r * dim, c.g * dim, c.b * dim, 1.0),
            );
            let label = if recipe.count > 1 {
                format!("{} x{}", recipe.output.data().name, recipe.count)
            } else {
                recipe.output.data().name.to_string()
            };
            draw_text(
                &label,
                r.x + 24.0,
                r.y + 17.0,
                18.0,
                if craftable { OK } else { NO },
            );

            if craftable && r.contains(mouse) && is_mouse_button_pressed(MouseButton::Left) {
                out.push(ClientMessage::Craft {
                    recipe_id: recipe.id,
                });
                self.flash = Some((recipe.id, now));
            }
        }
        if rows.is_empty() {
            draw_text("Nothing to craft here", x0 + 8.0, list_y + 20.0, 18.0, DIM);
        }
        if rows.len() > VISIBLE_ROWS {
            let frac = self.scroll as f32 / (rows.len() - VISIBLE_ROWS) as f32;
            let bar_h = list_h * (VISIBLE_ROWS as f32 / rows.len() as f32);
            draw_rectangle(
                x0 + width - 4.0,
                list_y + frac * (list_h - bar_h),
                3.0,
                bar_h,
                Color::new(1.0, 1.0, 1.0, 0.35),
            );
        }

        // ---- Recipe tooltip: inputs with have/need. ---------------------------------
        if let Some(recipe) = hovered {
            draw_recipe_tooltip(mouse, recipe, inv, stations);
        }
    }

    /// A `SlotChanged` arrived — the craft round-trip is complete.
    pub fn reconcile(&mut self) {
        self.flash = None;
    }
}

impl Default for CraftingUi {
    fn default() -> Self {
        CraftingUi::new()
    }
}

fn station_name(s: Station) -> &'static str {
    match s {
        Station::Hands => "By hand",
        Station::Workbench => "Workbench",
        Station::Furnace => "Furnace",
        Station::Anvil => "Anvil",
        Station::InfernalForge => "Infernal Forge",
        Station::RitualAltar => "Ritual Altar",
        Station::Bottle => "Placed Bottle",
    }
}

fn count_item(inv: &[Option<InvSlot>], item: ItemId) -> u64 {
    inv.iter()
        .flatten()
        .filter(|s| s.item == item)
        .map(|s| s.count as u64)
        .sum()
}

fn draw_recipe_tooltip(
    mouse: (f32, f32),
    recipe: &Recipe,
    inv: &[Option<InvSlot>],
    stations: StationSet,
) {
    let mut lines: Vec<(String, Color)> = Vec::with_capacity(recipe.inputs.len() + 2);
    lines.push((recipe.output.data().name.to_string(), TEXT));
    let station_ok = stations.contains(recipe.station);
    lines.push((
        format!(
            "{}{}",
            station_name(recipe.station),
            if station_ok { "" } else { " (not nearby)" }
        ),
        if station_ok { DIM } else { NO },
    ));
    for &(item, need) in recipe.inputs {
        let have = count_item(inv, item);
        let color = if have >= need as u64 { OK } else { NO };
        lines.push((format!("{} {}/{}", item.data().name, have, need), color));
    }

    let width = lines
        .iter()
        .map(|(l, _)| measure_text(l, None, 18, 1.0).width)
        .fold(90.0, f32::max)
        + 16.0;
    let height = lines.len() as f32 * 20.0 + 10.0;
    let x = (mouse.0 + 16.0).min(screen_width() - width - 4.0);
    let y = (mouse.1 - height - 6.0).max(4.0);
    draw_rectangle(x, y, width, height, Color::new(0.04, 0.05, 0.09, 0.93));
    draw_rectangle_lines(x, y, width, height, 1.5, Color::new(1.0, 1.0, 1.0, 0.18));
    for (i, (line, color)) in lines.iter().enumerate() {
        draw_text(line, x + 8.0, y + 20.0 + i as f32 * 20.0, 18.0, *color);
    }
}
