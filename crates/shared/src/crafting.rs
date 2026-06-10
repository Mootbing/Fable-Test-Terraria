//! Crafting stations, the static recipe table (DESIGN §4.4, all 70 recipes),
//! and pure helpers shared by server validation and the client crafting UI.

use serde::{Deserialize, Serialize};

use crate::items::{InvSlot, ItemId};
use crate::tiles::{state, TileId};
use crate::world::World;

/// Crafting stations (§4.4). A recipe is craftable when its station is
/// within [`crate::STATION_RANGE`] (4) tiles — note this is *not* the 6-tile
/// mine/place [`crate::REACH`]. "Hands" is always available; "Bottle" is a
/// bottle placed on a table/workbench — `tiles::state::BOTTLE_ON_TOP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Station {
    Hands,
    Workbench,
    Furnace,
    Anvil,
    InfernalForge,
    RitualAltar,
    Bottle,
}

/// Bitset of stations currently in reach. [`Station::Hands`] is always
/// considered present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StationSet(u8);

impl StationSet {
    pub const fn empty() -> StationSet {
        StationSet(0)
    }

    pub fn insert(&mut self, s: Station) {
        self.0 |= 1 << s as u8;
    }

    #[must_use]
    pub fn with(mut self, s: Station) -> StationSet {
        self.insert(s);
        self
    }

    pub fn contains(self, s: Station) -> bool {
        matches!(s, Station::Hands) || self.0 & (1 << s as u8) != 0
    }
}

/// One recipe row. `inputs` are (item, count) pairs, no duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    /// Stable id, matches the DESIGN §4.4 numbering (1–70). Sent in
    /// `ClientMessage::Craft`.
    pub id: u16,
    pub output: ItemId,
    pub count: u16,
    pub inputs: &'static [(ItemId, u16)],
    pub station: Station,
}

const fn r(
    id: u16,
    output: ItemId,
    count: u16,
    inputs: &'static [(ItemId, u16)],
    station: Station,
) -> Recipe {
    Recipe {
        id,
        output,
        count,
        inputs,
        station,
    }
}

use ItemId as I;
use Station as S;

/// All 70 recipes from DESIGN §4.4, ids matching the table there.
#[rustfmt::skip]
pub static RECIPES: &[Recipe] = &[
    // 1–11: basics at Hands/Workbench
    r(1, I::Workbench, 1, &[(I::Wood, 10)], S::Hands),
    r(2, I::Torch, 3, &[(I::Wood, 1), (I::Gel, 1)], S::Hands),
    r(3, I::WoodPlank, 1, &[(I::Wood, 1)], S::Hands),
    r(4, I::Platform, 2, &[(I::Wood, 1)], S::Workbench),
    r(5, I::Door, 1, &[(I::Wood, 6)], S::Workbench),
    r(6, I::Table, 1, &[(I::Wood, 8)], S::Workbench),
    r(7, I::Chair, 1, &[(I::Wood, 4)], S::Workbench),
    r(8, I::Chest, 1, &[(I::Wood, 8), (I::IronBar, 2)], S::Workbench),
    r(9, I::Bed, 1, &[(I::Wood, 15), (I::Cobweb, 20)], S::Workbench),
    r(10, I::WoodWall, 4, &[(I::Wood, 1)], S::Workbench),
    r(11, I::Furnace, 1, &[(I::Stone, 20), (I::Wood, 4), (I::Torch, 3)], S::Workbench),
    // 12–19: smelting & glass at the Furnace (17 at the Infernal Forge)
    r(12, I::StoneBrick, 2, &[(I::Stone, 2)], S::Furnace),
    r(13, I::CopperBar, 1, &[(I::CopperOre, 3)], S::Furnace),
    r(14, I::IronBar, 1, &[(I::IronOre, 3)], S::Furnace),
    r(15, I::SilverBar, 1, &[(I::SilverOre, 4)], S::Furnace),
    r(16, I::GoldBar, 1, &[(I::GoldOre, 4)], S::Furnace),
    r(17, I::EmberBar, 1, &[(I::Hellstone, 3), (I::Obsidian, 1)], S::InfernalForge),
    r(18, I::Glass, 1, &[(I::Sand, 2)], S::Furnace),
    r(19, I::Bottle, 2, &[(I::Glass, 1)], S::Furnace),
    r(20, I::Anvil, 1, &[(I::IronBar, 5)], S::Workbench),
    // 21–26: pickaxes
    r(21, I::WoodPickaxe, 1, &[(I::Wood, 12)], S::Workbench),
    r(22, I::CopperPickaxe, 1, &[(I::CopperBar, 8), (I::Wood, 4)], S::Anvil),
    r(23, I::IronPickaxe, 1, &[(I::IronBar, 10), (I::Wood, 4)], S::Anvil),
    r(24, I::SilverPickaxe, 1, &[(I::SilverBar, 10), (I::Wood, 4)], S::Anvil),
    r(25, I::GoldPickaxe, 1, &[(I::GoldBar, 10), (I::Wood, 4)], S::Anvil),
    r(26, I::EmberPickaxe, 1, &[(I::EmberBar, 20)], S::Anvil),
    // 27–32: axes
    r(27, I::WoodAxe, 1, &[(I::Wood, 9)], S::Workbench),
    r(28, I::CopperAxe, 1, &[(I::CopperBar, 9), (I::Wood, 3)], S::Anvil),
    r(29, I::IronAxe, 1, &[(I::IronBar, 9), (I::Wood, 3)], S::Anvil),
    r(30, I::SilverAxe, 1, &[(I::SilverBar, 9), (I::Wood, 3)], S::Anvil),
    r(31, I::GoldAxe, 1, &[(I::GoldBar, 9), (I::Wood, 3)], S::Anvil),
    r(32, I::EmberAxe, 1, &[(I::EmberBar, 15)], S::Anvil),
    // 33–38: swords
    r(33, I::WoodSword, 1, &[(I::Wood, 7)], S::Workbench),
    r(34, I::CopperSword, 1, &[(I::CopperBar, 8)], S::Anvil),
    r(35, I::IronSword, 1, &[(I::IronBar, 8)], S::Anvil),
    r(36, I::SilverSword, 1, &[(I::SilverBar, 8)], S::Anvil),
    r(37, I::GoldSword, 1, &[(I::GoldBar, 8)], S::Anvil),
    r(38, I::EmberBlade, 1, &[(I::EmberBar, 20)], S::Anvil),
    // 39–42: bows
    r(39, I::WoodenBow, 1, &[(I::Wood, 10)], S::Workbench),
    r(40, I::IronBow, 1, &[(I::IronBar, 7)], S::Anvil),
    r(41, I::GoldBow, 1, &[(I::GoldBar, 7)], S::Anvil),
    r(42, I::Cinderbow, 1, &[(I::EmberBar, 15)], S::Anvil),
    // 43–46: hammers & arrows
    r(43, I::WoodHammer, 1, &[(I::Wood, 8)], S::Workbench),
    r(44, I::IronHammer, 1, &[(I::IronBar, 8)], S::Anvil),
    r(45, I::WoodenArrow, 25, &[(I::Wood, 1), (I::Stone, 1)], S::Workbench),
    r(46, I::FlamingArrow, 10, &[(I::WoodenArrow, 10), (I::Torch, 1)], S::Hands),
    // 47–48: potions (placed bottle)
    r(47, I::LesserHealingPotion, 2, &[(I::Bottle, 2), (I::Gel, 2), (I::Mushroom, 1)], S::Bottle),
    r(48, I::HealingPotion, 1, &[(I::LesserHealingPotion, 2), (I::Gel, 1)], S::Bottle),
    // 49–66: armor (§4.2 — costs 15/25/20 bars per metal set; wood 20/30/25)
    r(49, I::WoodHelmet, 1, &[(I::Wood, 20)], S::Workbench),
    r(50, I::WoodChestplate, 1, &[(I::Wood, 30)], S::Workbench),
    r(51, I::WoodGreaves, 1, &[(I::Wood, 25)], S::Workbench),
    r(52, I::CopperHelmet, 1, &[(I::CopperBar, 15)], S::Anvil),
    r(53, I::CopperChestplate, 1, &[(I::CopperBar, 25)], S::Anvil),
    r(54, I::CopperGreaves, 1, &[(I::CopperBar, 20)], S::Anvil),
    r(55, I::IronHelmet, 1, &[(I::IronBar, 15)], S::Anvil),
    r(56, I::IronChestplate, 1, &[(I::IronBar, 25)], S::Anvil),
    r(57, I::IronGreaves, 1, &[(I::IronBar, 20)], S::Anvil),
    r(58, I::SilverHelmet, 1, &[(I::SilverBar, 15)], S::Anvil),
    r(59, I::SilverChestplate, 1, &[(I::SilverBar, 25)], S::Anvil),
    r(60, I::SilverGreaves, 1, &[(I::SilverBar, 20)], S::Anvil),
    r(61, I::GoldHelmet, 1, &[(I::GoldBar, 15)], S::Anvil),
    r(62, I::GoldChestplate, 1, &[(I::GoldBar, 25)], S::Anvil),
    r(63, I::GoldGreaves, 1, &[(I::GoldBar, 20)], S::Anvil),
    r(64, I::EmberHelmet, 1, &[(I::EmberBar, 10)], S::Anvil),
    r(65, I::EmberChestplate, 1, &[(I::EmberBar, 20)], S::Anvil),
    r(66, I::EmberGreaves, 1, &[(I::EmberBar, 15)], S::Anvil),
    // 67–70: crowns & boss summons
    r(67, I::GoldCrown, 1, &[(I::GoldBar, 5)], S::Anvil),
    r(68, I::GelCrown, 1, &[(I::GoldCrown, 1), (I::Gel, 20)], S::RitualAltar),
    r(69, I::WatchersIris, 1, &[(I::Lens, 6)], S::RitualAltar),
    r(70, I::CursedEffigy, 1, &[(I::Bone, 30), (I::GoldBar, 10)], S::RitualAltar),
];

/// Looks a recipe up by its stable id.
pub fn recipe_by_id(id: u16) -> Option<&'static Recipe> {
    // Ids are contiguous from 1, but don't rely on it.
    RECIPES.iter().find(|r| r.id == id)
}

/// Total count of `item` across a slot array.
fn count_item(slots: &[Option<InvSlot>], item: ItemId) -> u64 {
    slots
        .iter()
        .flatten()
        .filter(|s| s.item == item)
        .map(|s| s.count as u64)
        .sum()
}

/// All recipe inputs are present in `slots` (station not considered).
pub fn can_craft(recipe: &Recipe, slots: &[Option<InvSlot>]) -> bool {
    recipe
        .inputs
        .iter()
        .all(|&(item, need)| count_item(slots, item) >= need as u64)
}

/// Recipes whose station is in `stations` and whose ingredients are all in
/// `slots` — the crafting-UI list (§4.4).
pub fn recipes_available(
    stations: StationSet,
    slots: &[Option<InvSlot>],
) -> impl Iterator<Item = &'static Recipe> + '_ {
    RECIPES
        .iter()
        .filter(move |r| stations.contains(r.station) && can_craft(r, slots))
}

/// Crafting stations within [`crate::STATION_RANGE`] (4 tiles, §4.4) of
/// `center` (the player's center, tile units): every tile cell whose own
/// center is in range and that provides a station counts — so being near
/// *any* tile of a multi-tile station is enough. A Bottle placed on a
/// Table/Workbench cell ([`state::BOTTLE_ON_TOP`]) provides
/// [`Station::Bottle`].
///
/// Shared so the server (Craft validation) and the client (recipe-list
/// filtering) agree exactly; the server stays the authority.
pub fn stations_in_range(world: &World, center: (f32, f32)) -> StationSet {
    let mut stations = StationSet::empty();
    let r = crate::STATION_RANGE;
    let x0 = (center.0 - r).floor().max(0.0) as u32;
    let y0 = (center.1 - r).floor().max(0.0) as u32;
    let x1 = ((center.0 + r).ceil() as u32).min(world.width.saturating_sub(1));
    let y1 = ((center.1 + r).ceil() as u32).min(world.height.saturating_sub(1));
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (dx, dy) = (x as f32 + 0.5 - center.0, y as f32 + 0.5 - center.1);
            if dx * dx + dy * dy > r * r {
                continue;
            }
            let t = world.tile(x, y);
            if let Some(s) = t.id.data().station {
                stations.insert(s);
            }
            // The server sets BOTTLE_ON_TOP when a Bottle item is placed
            // onto a Table/Workbench cell (`sim::interact::place_bottle`).
            if matches!(t.id, TileId::Table | TileId::Workbench)
                && t.state & state::BOTTLE_ON_TOP != 0
            {
                stations.insert(Station::Bottle);
            }
        }
    }
    stations
}

/// Removes `count` of `item` from the slots (which must contain enough).
fn consume(slots: &mut [Option<InvSlot>], item: ItemId, count: u16) {
    let mut left = count;
    for slot in slots.iter_mut() {
        if left == 0 {
            break;
        }
        if let Some(s) = slot {
            if s.item == item {
                let take = s.count.min(left);
                s.count -= take;
                left -= take;
                if s.count == 0 {
                    *slot = None;
                }
            }
        }
    }
}

/// Adds `count` of `item`, stacking onto existing stacks first, then into
/// empty slots. Returns the count that did not fit (0 = everything placed;
/// `slots` is left modified either way — callers handle transactionality).
fn insert(slots: &mut [Option<InvSlot>], item: ItemId, count: u16) -> u16 {
    let max = item.max_stack();
    let mut left = count;
    for slot in slots.iter_mut() {
        if left == 0 {
            return 0;
        }
        if let Some(s) = slot {
            if s.item == item && s.count < max {
                let add = (max - s.count).min(left);
                s.count += add;
                left -= add;
            }
        }
    }
    for slot in slots.iter_mut() {
        if left == 0 {
            return 0;
        }
        if slot.is_none() {
            let add = max.min(left);
            *slot = Some(InvSlot::new(item, add));
            left -= add;
        }
    }
    left
}

/// Crafts `recipe` against `slots`: consumes inputs and inserts the output.
/// Transactional — on `false` (missing ingredients or no room for the
/// output) `slots` is unchanged. The station check is the caller's job.
pub fn apply_craft(recipe: &Recipe, slots: &mut [Option<InvSlot>]) -> bool {
    matches!(apply_craft_overflow(recipe, slots, false), Some(0))
}

/// Like [`apply_craft`], but with `allow_overflow` the craft succeeds even
/// when the output doesn't fully fit: inputs are consumed, what fits is
/// inserted, and the overflow count is returned for the caller to drop on
/// the ground (§4.4 crafting always yields its output). Returns `None`
/// (slots unchanged) when ingredients are missing — or when overflow is
/// disallowed and the output has no room.
pub fn apply_craft_overflow(
    recipe: &Recipe,
    slots: &mut [Option<InvSlot>],
    allow_overflow: bool,
) -> Option<u16> {
    if !can_craft(recipe, slots) {
        return None;
    }
    let mut work = slots.to_vec();
    for &(item, need) in recipe.inputs {
        consume(&mut work, item, need);
    }
    let overflow = insert(&mut work, recipe.output, recipe.count);
    if overflow > 0 && !allow_overflow {
        return None;
    }
    slots.copy_from_slice(&work);
    Some(overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(items: &[(ItemId, u16)]) -> Vec<Option<InvSlot>> {
        let mut slots = vec![None; 50];
        for (i, &(item, count)) in items.iter().enumerate() {
            slots[i] = Some(InvSlot::new(item, count));
        }
        slots
    }

    #[test]
    fn recipe_table_is_complete() {
        assert_eq!(RECIPES.len(), 70);
        for (i, r) in RECIPES.iter().enumerate() {
            assert_eq!(r.id as usize, i + 1, "ids are contiguous from 1");
            assert!(r.count > 0);
            assert!(!r.inputs.is_empty());
        }
        assert!(recipe_by_id(0).is_none());
        assert!(recipe_by_id(71).is_none());
        assert_eq!(recipe_by_id(45).expect("recipe").count, 25); // arrows ×25
    }

    #[test]
    fn armor_recipe_count_is_18() {
        let armor = RECIPES
            .iter()
            .filter(|r| {
                let d = r.output.data();
                d.armor.is_some_and(|a| a.set.is_some())
            })
            .count();
        assert_eq!(armor, 18);
    }

    #[test]
    fn boss_summons_require_ritual_altar() {
        for item in [I::GelCrown, I::WatchersIris, I::CursedEffigy] {
            let recipe = RECIPES
                .iter()
                .find(|r| r.output == item)
                .expect("summon recipe");
            assert_eq!(recipe.station, Station::RitualAltar, "{item:?}");
        }
        // And nothing else does.
        let at_altar = RECIPES
            .iter()
            .filter(|r| r.station == Station::RitualAltar)
            .count();
        assert_eq!(at_altar, 3);
    }

    #[test]
    fn torch_recipe_math() {
        let mut slots = inv(&[(I::Wood, 2), (I::Gel, 1)]);
        let torch = recipe_by_id(2).expect("torch recipe");
        assert!(can_craft(torch, &slots));
        assert!(apply_craft(torch, &mut slots));
        assert_eq!(count_item(&slots, I::Torch), 3);
        assert_eq!(count_item(&slots, I::Wood), 1);
        assert_eq!(count_item(&slots, I::Gel), 0);
        // Gel is gone: a second craft must fail and change nothing.
        let before = slots.clone();
        assert!(!apply_craft(torch, &mut slots));
        assert_eq!(slots, before);
    }

    #[test]
    fn bar_ratios() {
        let copper = recipe_by_id(13).expect("copper bar");
        assert_eq!(copper.inputs, &[(I::CopperOre, 3)]);
        let silver = recipe_by_id(15).expect("silver bar");
        assert_eq!(silver.inputs, &[(I::SilverOre, 4)]);

        let mut slots = inv(&[(I::CopperOre, 7)]);
        assert!(apply_craft(copper, &mut slots)); // 7 -> 4 ore + 1 bar
        assert!(apply_craft(copper, &mut slots)); // 4 -> 1 ore + 2 bars
        assert!(!can_craft(copper, &slots), "1 ore is not enough");
        assert_eq!(count_item(&slots, I::CopperBar), 2);
        assert_eq!(count_item(&slots, I::CopperOre), 1);
    }

    #[test]
    fn inputs_split_across_stacks_are_counted() {
        let mut slots = inv(&[(I::Wood, 5), (I::Wood, 5)]);
        let workbench = recipe_by_id(1).expect("workbench");
        assert!(can_craft(workbench, &slots));
        assert!(apply_craft(workbench, &mut slots));
        assert_eq!(count_item(&slots, I::Wood), 0);
        assert_eq!(count_item(&slots, I::Workbench), 1);
    }

    #[test]
    fn craft_fails_when_output_has_no_room() {
        // Two slots, both full after consuming: 1 wood + 1 stone -> arrows,
        // but a full unrelated stack blocks the output.
        let mut slots = vec![
            Some(InvSlot::new(I::Wood, 2)), // 1 left after craft
            Some(InvSlot::new(I::Stone, 2)),
        ];
        let arrows = recipe_by_id(45).expect("arrows");
        let before = slots.clone();
        assert!(!apply_craft(arrows, &mut slots), "no empty slot for output");
        assert_eq!(slots, before, "transactional on failure");

        // Freeing a slot by consuming the whole stack makes it succeed.
        let mut slots = vec![
            Some(InvSlot::new(I::Wood, 1)), // fully consumed -> empties
            Some(InvSlot::new(I::Stone, 2)),
        ];
        assert!(apply_craft(arrows, &mut slots));
        assert_eq!(count_item(&slots, I::WoodenArrow), 25);
    }

    #[test]
    fn craft_overflow_consumes_and_reports_the_excess() {
        // 25 arrows out, but only 990..999 fits onto the existing stack.
        let arrows = recipe_by_id(45).expect("arrows");
        let mut slots = vec![
            Some(InvSlot::new(I::Wood, 2)),
            Some(InvSlot::new(I::Stone, 2)),
            Some(InvSlot::new(I::WoodenArrow, 990)),
        ];
        let over = apply_craft_overflow(arrows, &mut slots, true).expect("crafts");
        assert_eq!(over, 16, "9 fit on the stack, 16 overflow");
        assert_eq!(count_item(&slots, I::WoodenArrow), 999);
        assert_eq!(count_item(&slots, I::Wood), 1);
        // Missing inputs still fail without touching anything.
        let mut empty: Vec<Option<InvSlot>> = vec![None; 5];
        assert_eq!(apply_craft_overflow(arrows, &mut empty, true), None);
    }

    #[test]
    fn stations_detected_within_range_4() {
        use crate::tiles::Tile;
        let mut w = World::new(64, 64);
        // 2×1 workbench at (10, 10); 3×2 furnace at (30, 10).
        assert!(w.place_multitile(10, 10, TileId::Workbench));
        assert!(w.place_multitile(30, 10, TileId::Furnace));

        // Standing right next to the bench: in range. Center of cell (10,10)
        // is (10.5, 10.5).
        let near = stations_in_range(&w, (12.0, 10.5));
        assert!(near.contains(Station::Workbench));
        assert!(near.contains(Station::Hands), "hands always available");
        assert!(!near.contains(Station::Furnace));

        // 4 tiles from the bench's right cell (11,10): center distance from
        // (15.5, 10.5) to (11.5, 10.5) is exactly 4 — any tile of the
        // station counts.
        assert!(stations_in_range(&w, (15.5, 10.5)).contains(Station::Workbench));
        // A hair past 4 from every cell: out of range.
        assert!(!stations_in_range(&w, (16.2, 10.5)).contains(Station::Workbench));

        // Bottle on a table enables the Bottle station (§4.4).
        assert!(w.place_multitile(20, 20, TileId::Table));
        assert!(!stations_in_range(&w, (21.0, 21.0)).contains(Station::Bottle));
        let mut t = w.tile(20, 20);
        t.state |= state::BOTTLE_ON_TOP;
        w.set_tile(20, 20, t);
        assert!(stations_in_range(&w, (21.0, 21.0)).contains(Station::Bottle));
        // ...but DOOR_OPEN shares the bit and must not leak from doors.
        let mut door = Tile::of(TileId::Door);
        door.state |= state::DOOR_OPEN;
        w.set_tile(40, 40, door);
        assert!(!stations_in_range(&w, (40.5, 40.5)).contains(Station::Bottle));
    }

    #[test]
    fn station_filtering() {
        let slots = inv(&[(I::Wood, 100), (I::Gel, 5), (I::Stone, 50)]);
        let hands_only: Vec<u16> = recipes_available(StationSet::empty(), &slots)
            .map(|r| r.id)
            .collect();
        assert!(hands_only.contains(&1)); // workbench (Hands)
        assert!(hands_only.contains(&2)); // torch (Hands)
        assert!(!hands_only.contains(&4), "platform needs a workbench");

        let mut near_bench = StationSet::empty();
        near_bench.insert(Station::Workbench);
        let with_bench: Vec<u16> = recipes_available(near_bench, &slots)
            .map(|r| r.id)
            .collect();
        assert!(with_bench.contains(&4));
        assert!(with_bench.contains(&45)); // arrows: wood + stone at bench
        assert!(!with_bench.contains(&12), "stone brick needs a furnace");
        assert!(near_bench.contains(Station::Hands), "hands always present");
        assert!(!near_bench.contains(Station::Anvil));
    }
}
