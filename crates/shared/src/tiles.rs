//! Tile and wall definitions plus the per-cell [`Tile`] struct.
//!
//! All numbers come from DESIGN.md §2 (tiles) and §10 (lighting). The 35-row
//! tile table in DESIGN splits here into [`TileId`] (rows 1–32, the
//! foreground layer, plus `Air`) and [`WallId`] (rows 33–35, the background
//! wall layer) because walls live in a separate layer of every cell.

use serde::{Deserialize, Serialize};

use crate::crafting::Station;
use crate::items::ItemId;
use crate::macros::id_table;

/// Hardness multipliers (§2): tool swings deal `tool_power × multiplier`
/// break-points to a tile.
pub const MULT_SOFT: f32 = 2.0;
pub const MULT_MEDIUM: f32 = 1.0;
pub const MULT_HARD: f32 = 0.75;
pub const MULT_VERY_HARD: f32 = 0.5;

/// Every tile has this many break-points; accumulated damage resets after
/// [`TILE_DAMAGE_RESET_SECS`] without hits (§2).
pub const TILE_BREAK_POINTS: u32 = 100;
pub const TILE_DAMAGE_RESET_SECS: f32 = 5.0;

/// Grass spreads to adjacent air-exposed dirt with 1-in-N chance per tile
/// update (§2, tile 3).
pub const GRASS_SPREAD_DENOM: u32 = 600;

/// Tree segments drop 10 wood each, plus a 25% chance of 1 acorn (§2 tile 32).
pub const WOOD_PER_TREE_SEGMENT: u8 = 10;
pub const ACORN_DROP_CHANCE: f32 = 0.25;

/// Sapling growth window (§2 tile 31): grows to a tree after 5–10 minutes if
/// at least 7 air tiles are above it.
pub const SAPLING_GROW_MIN_SECS: f32 = 300.0;
pub const SAPLING_GROW_MAX_SECS: f32 = 600.0;
pub const SAPLING_AIR_NEEDED: u32 = 7;

/// Trees are 7–16 trunk segments tall (§1.2 pass 9; grown saplings match
/// world-gen trees).
pub const TREE_HEIGHT_MIN: u32 = 7;
pub const TREE_HEIGHT_MAX: u32 = 16;

/// Hammering a Ritual Altar deals 50% of the striker's current HP (§2).
pub const RITUAL_ALTAR_BACKLASH_HP_FRACTION: f32 = 0.5;

// ---- Lighting constants (§10; lighting itself is client-side) -------------

/// Light levels are 0–32 per tile.
pub const LIGHT_MAX: u8 = 32;
pub const SKY_LIGHT_DAY: u8 = 32;
pub const SKY_LIGHT_NIGHT: u8 = 8;
/// Sky light interpolates linearly across the 30 in-game minutes (1800
/// ticks) centered on dawn/dusk (§9/§10).
pub const SKY_LIGHT_RAMP_TICKS: u32 = 1800;
/// BFS attenuation per step entering a non-solid / solid tile.
pub const LIGHT_ATTEN_AIR: u8 = 2;
pub const LIGHT_ATTEN_SOLID: u8 = 6;
/// Non-tile light sources (tile-bound sources live in [`TILE_DATA`]).
pub const LAVA_LIGHT: u8 = 18;
pub const PLAYER_GLOW: u8 = 4;
pub const MINING_HELMET_LIGHT: u8 = 20;

/// How a tile blocks movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Solidity {
    /// Blocks from every side.
    Solid,
    /// Solid from above only; Down+Jump drops through (§2 tile 17).
    Platform,
    /// Never blocks movement.
    NotSolid,
}

/// Which tool class hits a tile, wall, or is a tool item's class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolKind {
    Pick,
    Axe,
    Hammer,
    /// Breakable by anything (torches, pots, cobwebs).
    Any,
    /// Not breakable by tools at all.
    None,
}

/// Static per-tile-type data; one row per [`TileId`] in [`TILE_DATA`].
#[derive(Debug)]
pub struct TileData {
    pub name: &'static str,
    pub solidity: Solidity,
    /// Tool class required to break it.
    pub tool: ToolKind,
    /// Material multiplier for the mining model (§2). Irrelevant when
    /// `one_hit` is set.
    pub hardness_mult: f32,
    /// Minimum tool power; below it the tile takes zero damage.
    pub min_power: u8,
    /// Emitted light, 0–32 (§10).
    pub light: u8,
    /// What breaking it drops. `None` = nothing fixed (Pot loot and the
    /// per-segment acorn roll are server-side loot logic).
    pub drops: Option<(ItemId, u8)>,
    /// Breaks in a single hit from a matching tool (furniture ✦, pots, ...).
    pub one_hit: bool,
    /// `false` only for the Ritual Altar.
    pub breakable: bool,
    /// Furniture per §2 (✦).
    pub furniture: bool,
    /// Crafting station provided when within `crate::STATION_RANGE` (4
    /// tiles, §4.4).
    pub station: Option<Station>,
    /// Multi-tile footprint in tiles (w, h); `(1, 1)` for single tiles.
    pub size: (u8, u8),
}

impl TileData {
    const fn new(name: &'static str) -> Self {
        TileData {
            name,
            solidity: Solidity::NotSolid,
            tool: ToolKind::None,
            hardness_mult: 0.0,
            min_power: 0,
            light: 0,
            drops: None,
            one_hit: false,
            breakable: true,
            furniture: false,
            station: None,
            size: (1, 1),
        }
    }

    /// A plain solid pick-mined block dropping one of `drop`.
    const fn block(name: &'static str, mult: f32, drop: ItemId) -> Self {
        let mut d = Self::new(name);
        d.solidity = Solidity::Solid;
        d.tool = ToolKind::Pick;
        d.hardness_mult = mult;
        d.drops = Some((drop, 1));
        d
    }

    /// Furniture (✦): non-solid, one pick hit, drops its item.
    const fn furniture(name: &'static str, drop: ItemId, w: u8, h: u8) -> Self {
        let mut d = Self::new(name);
        d.tool = ToolKind::Pick;
        d.one_hit = true;
        d.furniture = true;
        d.drops = Some((drop, 1));
        d.size = (w, h);
        d
    }

    const fn solidity(mut self, s: Solidity) -> Self {
        self.solidity = s;
        self
    }
    const fn tool(mut self, t: ToolKind) -> Self {
        self.tool = t;
        self
    }
    const fn hardness(mut self, m: f32) -> Self {
        self.hardness_mult = m;
        self
    }
    const fn min_power(mut self, p: u8) -> Self {
        self.min_power = p;
        self
    }
    const fn light(mut self, l: u8) -> Self {
        self.light = l;
        self
    }
    const fn drops(mut self, item: ItemId, n: u8) -> Self {
        self.drops = Some((item, n));
        self
    }
    const fn no_drop(mut self) -> Self {
        self.drops = None;
        self
    }
    const fn one_hit(mut self) -> Self {
        self.one_hit = true;
        self
    }
    const fn unbreakable(mut self) -> Self {
        self.breakable = false;
        self
    }
    const fn station(mut self, s: Station) -> Self {
        self.station = Some(s);
        self
    }
    const fn size(mut self, w: u8, h: u8) -> Self {
        self.size = (w, h);
        self
    }
}

id_table! {
    /// Foreground tile layer ids (DESIGN §2 rows 1–32, plus `Air` = 0).
    /// Discriminants match the DESIGN table ids exactly.
    pub enum TileId(u8), pub table TILE_DATA: TileData {
        Air => TileData::new("Air"),
        Dirt => TileData::block("Dirt", MULT_SOFT, ItemId::Dirt),
        Stone => TileData::block("Stone", MULT_MEDIUM, ItemId::Stone),
        /// Spreads to adjacent air-exposed dirt; dies if covered (server).
        Grass => TileData::block("Grass", MULT_SOFT, ItemId::Dirt),
        /// Falls when unsupported (server turns it into a falling entity).
        Sand => TileData::block("Sand", MULT_SOFT, ItemId::Sand),
        Clay => TileData::block("Clay", MULT_SOFT, ItemId::Clay),
        WoodPlank => TileData::block("Wood Plank", MULT_MEDIUM, ItemId::WoodPlank),
        CopperOre => TileData::block("Copper Ore", MULT_MEDIUM, ItemId::CopperOre),
        IronOre => TileData::block("Iron Ore", MULT_MEDIUM, ItemId::IronOre),
        SilverOre => TileData::block("Silver Ore", MULT_HARD, ItemId::SilverOre),
        GoldOre => TileData::block("Gold Ore", MULT_HARD, ItemId::GoldOre),
        /// Touching it while not fire-immune inflicts Burning 2 s (server).
        Hellstone => TileData::block("Hellstone", MULT_VERY_HARD, ItemId::Hellstone)
            .min_power(55)
            .light(13),
        Obsidian => TileData::block("Obsidian", MULT_VERY_HARD, ItemId::Obsidian).min_power(55),
        Ash => TileData::block("Ash", MULT_SOFT, ItemId::Ash),
        StoneBrick => TileData::block("Stone Brick", MULT_MEDIUM, ItemId::StoneBrick),
        EmberBrick => TileData::block("Ember Brick", MULT_HARD, ItemId::EmberBrick),
        /// Attaches to a solid tile/wall; extinguishes in water (server).
        Torch => TileData::furniture("Torch", ItemId::Torch, 1, 1)
            .tool(ToolKind::Any)
            .light(28),
        Platform => TileData::block("Platform", MULT_SOFT, ItemId::Platform)
            .solidity(Solidity::Platform),
        /// 1×3. Solid while closed; see [`state::DOOR_OPEN`]. Needs solid
        /// tiles above and below.
        Door => TileData::furniture("Door", ItemId::Door, 1, 3).solidity(Solidity::Solid),
        /// 2×2, 40 slots; can't break while non-empty (server rule).
        Chest => TileData::furniture("Chest", ItemId::Chest, 2, 2),
        Workbench => TileData::furniture("Workbench", ItemId::Workbench, 2, 1)
            .station(Station::Workbench),
        Furnace => TileData::furniture("Furnace", ItemId::Furnace, 3, 2)
            .light(6)
            .station(Station::Furnace),
        Anvil => TileData::furniture("Anvil", ItemId::Anvil, 2, 1).station(Station::Anvil),
        /// World-gen only, but relocatable: needs pick power 55 to free.
        InfernalForge => TileData::furniture("Infernal Forge", ItemId::InfernalForge, 3, 2)
            .min_power(55)
            .light(10)
            .station(Station::InfernalForge),
        /// Unbreakable; hammering it backfires (see
        /// [`RITUAL_ALTAR_BACKLASH_HP_FRACTION`]).
        RitualAltar => TileData::new("Ritual Altar")
            .unbreakable()
            .light(8)
            .station(Station::RitualAltar)
            .size(3, 2),
        Table => TileData::furniture("Table", ItemId::Table, 3, 2),
        Chair => TileData::furniture("Chair", ItemId::Chair, 1, 2),
        /// Right-click sets personal spawn (server).
        Bed => TileData::furniture("Bed", ItemId::Bed, 4, 2),
        /// Drops from the pot loot table (§2.3), rolled server-side.
        Pot => TileData::new("Pot").tool(ToolKind::Any).one_hit().no_drop(),
        LifeCrystal => TileData::new("Life Crystal")
            .tool(ToolKind::Pick)
            .one_hit()
            .light(6)
            .drops(ItemId::LifeCrystal, 1),
        /// Entities inside are slowed (see `physics::COBWEB_MAX_SPEED`).
        Cobweb => TileData::new("Cobweb")
            .tool(ToolKind::Any)
            .one_hit()
            .drops(ItemId::Cobweb, 1),
        Sapling => TileData::new("Sapling").tool(ToolKind::Axe).one_hit().no_drop(),
        /// Background, not solid. Drops are per segment; felling logic and
        /// the 25% acorn roll are server-side ([`WOOD_PER_TREE_SEGMENT`],
        /// [`ACORN_DROP_CHANCE`]).
        TreeTrunk => TileData::new("Tree Trunk")
            .tool(ToolKind::Axe)
            .hardness(MULT_MEDIUM)
            .drops(ItemId::Wood, WOOD_PER_TREE_SEGMENT),
    }
}

/// Static per-wall-type data; one row per [`WallId`] in [`WALL_DATA`].
/// Walls are removed with hammers. Naturally generated dirt/stone walls drop
/// nothing — only player-placed ones do (tracked via [`state::WALL_PLACED`]).
#[derive(Debug)]
pub struct WallData {
    pub name: &'static str,
    pub hardness_mult: f32,
    pub drops: Option<ItemId>,
}

impl WallData {
    const fn new(name: &'static str, mult: f32, drops: Option<ItemId>) -> Self {
        WallData {
            name,
            hardness_mult: mult,
            drops,
        }
    }
}

id_table! {
    /// Background wall layer ids (DESIGN §2 rows 33–35, plus `Air` = 0).
    pub enum WallId(u8), pub table WALL_DATA: WallData {
        Air => WallData::new("No Wall", 0.0, None),
        Dirt => WallData::new("Dirt Wall", MULT_SOFT, Some(ItemId::DirtWall)),
        Stone => WallData::new("Stone Wall", MULT_MEDIUM, Some(ItemId::StoneWall)),
        /// Craftable; counts as a "safe" wall for housing.
        Wood => WallData::new("Wood Wall", MULT_SOFT, Some(ItemId::WoodWall)),
    }
}

/// Fluid kind stored in a cell (§3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiquidKind {
    Water,
    Lava,
}

/// Fluid levels run 1–8 per cell; 8 = full (§3).
pub const LIQUID_MAX_LEVEL: u8 = 8;
/// Update cadence in ticks: water every 2, lava every 5 (§3).
pub const WATER_UPDATE_TICKS: u32 = 2;
pub const LAVA_UPDATE_TICKS: u32 = 5;
/// Level-1 puddles on flat ground evaporate after 60 s (§3).
pub const PUDDLE_EVAPORATE_SECS: f32 = 60.0;
/// Lava contact: 50 damage + Burning for 7 s (§3).
pub const LAVA_CONTACT_DAMAGE: u32 = 50;
pub const LAVA_BURN_SECS: f32 = 7.0;

/// Per-cell liquid, packed into one byte: bits 0–3 level (0–8), bits 4–5
/// kind (0 = none, 1 = water, 2 = lava).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Liquid(u8);

impl Liquid {
    pub const NONE: Liquid = Liquid(0);

    /// A liquid cell; `level` is clamped to 1..=8.
    pub fn new(kind: LiquidKind, level: u8) -> Liquid {
        let level = level.clamp(1, LIQUID_MAX_LEVEL);
        let k = match kind {
            LiquidKind::Water => 1u8,
            LiquidKind::Lava => 2u8,
        };
        Liquid((k << 4) | level)
    }

    #[inline]
    pub fn kind(self) -> Option<LiquidKind> {
        match self.0 >> 4 {
            1 => Some(LiquidKind::Water),
            2 => Some(LiquidKind::Lava),
            _ => None,
        }
    }

    /// Fill level 0–8 (0 iff no liquid).
    #[inline]
    pub fn level(self) -> u8 {
        self.0 & 0x0f
    }

    #[inline]
    pub fn is_none(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub fn is_some(self) -> bool {
        self.0 != 0
    }

    #[inline]
    pub fn raw(self) -> u8 {
        self.0
    }

    /// Validates a raw byte (used by chunk decoding).
    pub fn from_raw(raw: u8) -> Option<Liquid> {
        let kind = raw >> 4;
        let level = raw & 0x0f;
        match (kind, level) {
            (0, 0) => Some(Liquid::NONE),
            (1 | 2, 1..=LIQUID_MAX_LEVEL) => Some(Liquid(raw)),
            _ => None,
        }
    }
}

/// Meanings of [`Tile::state`], the per-cell frame/state byte.
///
/// Layout:
/// - bits 0–2: multi-tile part x offset ([`part_x`]), or the tree segment
///   kind for `TreeTrunk` ([`TREE_SEGMENT_TRUNK`]/[`TREE_SEGMENT_TOP`]), or a
///   sprite variant for plain tiles (grass decoration etc.).
/// - bits 3–4: multi-tile part y offset ([`part_y`]).
/// - bit 5: [`DOOR_OPEN`] on doors; [`BOTTLE_ON_TOP`] on tables/workbenches.
/// - bit 6: [`DOOR_OPEN_LEFT`] on open doors (panel side).
/// - bit 7: [`WALL_PLACED`] — the wall in this cell was player-placed and
///   drops its item when hammered (natural walls drop nothing).
pub mod state {
    /// Door is open (not solid). Doors are 1×3; part offsets still apply.
    pub const DOOR_OPEN: u8 = 1 << 5;
    /// The open door's panel swings against the left jamb (away from the
    /// player who toggled it); unset = right. Render-side only — the door
    /// column itself is the non-solid passage either way.
    pub const DOOR_OPEN_LEFT: u8 = 1 << 6;
    /// A Bottle sits on this Table/Workbench cell, enabling the
    /// `Station::Bottle` crafting station (§4.4).
    pub const BOTTLE_ON_TOP: u8 = 1 << 5;
    /// The wall in this cell was placed by a player (affects wall drops).
    pub const WALL_PLACED: u8 = 1 << 7;

    /// Tree segment kinds (bits 0–2 of a `TreeTrunk` tile).
    pub const TREE_SEGMENT_TRUNK: u8 = 0;
    pub const TREE_SEGMENT_TOP: u8 = 1;

    /// Sprite-variant (bits 0–2) on a `Grass` tile: a mushroom forage plant
    /// grows here (DESIGN §1.2 pass 9 — the 35-tile table has no mushroom
    /// tile, so forage plants live on the grass cell; foraging yields
    /// `ItemId::Mushroom` and clears the variant, handled server-side).
    pub const GRASS_MUSHROOM: u8 = 1;

    /// The sprite-variant bits (0–2) of a 1×1 tile's state byte — the same
    /// bits multi-tile parts use for [`part_x`].
    pub const VARIANT_MASK: u8 = 0x7;

    /// Sprite variant of a 1×1 tile (grass mushroom, tree segment kind).
    #[inline]
    pub const fn variant(state: u8) -> u8 {
        state & VARIANT_MASK
    }

    /// Packs a multi-tile part offset. `dx` 0–7, `dy` 0–3.
    #[inline]
    pub const fn part(dx: u8, dy: u8) -> u8 {
        (dx & 0x7) | ((dy & 0x3) << 3)
    }

    /// X offset of this cell within its multi-tile object.
    #[inline]
    pub const fn part_x(state: u8) -> u8 {
        state & 0x7
    }

    /// Y offset of this cell within its multi-tile object.
    #[inline]
    pub const fn part_y(state: u8) -> u8 {
        (state >> 3) & 0x3
    }
}

/// One world cell: foreground tile, background wall, liquid, and a
/// frame/state byte (see [`state`]). Exactly 4 bytes, `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tile {
    pub id: TileId,
    pub wall: WallId,
    pub liquid: Liquid,
    pub state: u8,
}

const _: () = assert!(
    std::mem::size_of::<Tile>() == 4,
    "Tile must stay <= 4 bytes"
);

impl Tile {
    pub const AIR: Tile = Tile {
        id: TileId::Air,
        wall: WallId::Air,
        liquid: Liquid::NONE,
        state: 0,
    };

    /// A bare tile of `id` with no wall, liquid, or state.
    pub const fn of(id: TileId) -> Tile {
        Tile {
            id,
            wall: WallId::Air,
            liquid: Liquid::NONE,
            state: 0,
        }
    }

    /// Fully solid right now (closed doors are solid, open ones aren't;
    /// platforms are *not* fully solid — see [`Tile::is_platform`]).
    #[inline]
    pub fn is_solid(self) -> bool {
        match self.id {
            TileId::Door => self.state & state::DOOR_OPEN == 0,
            id => matches!(id.data().solidity, Solidity::Solid),
        }
    }

    /// Solid from above only.
    #[inline]
    pub fn is_platform(self) -> bool {
        matches!(self.id.data().solidity, Solidity::Platform)
    }

    /// Raw little-endian byte layout used by chunk encoding:
    /// `[tile id, wall id, liquid byte, state byte]`.
    #[inline]
    pub fn to_bytes(self) -> [u8; 4] {
        [
            self.id as u8,
            self.wall as u8,
            self.liquid.raw(),
            self.state,
        ]
    }

    /// Inverse of [`Tile::to_bytes`]; `None` if any field is invalid.
    pub fn from_bytes(b: [u8; 4]) -> Option<Tile> {
        Some(Tile {
            id: TileId::from_repr(b[0])?,
            wall: WallId::from_repr(b[1])?,
            liquid: Liquid::from_raw(b[2])?,
            state: b[3],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_every_tile_id() {
        assert_eq!(TILE_DATA.len(), TileId::COUNT);
        assert_eq!(TileId::COUNT, 33); // Air + 32 foreground tiles
        assert_eq!(WALL_DATA.len(), WallId::COUNT);
        assert_eq!(WallId::COUNT, 4);
        for id in TileId::ALL {
            assert!(!id.data().name.is_empty());
        }
    }

    #[test]
    fn design_ids_match_discriminants() {
        // DESIGN §2 numbers a few we rely on elsewhere.
        assert_eq!(TileId::Dirt as u8, 1);
        assert_eq!(TileId::Hellstone as u8, 11);
        assert_eq!(TileId::Torch as u8, 16);
        assert_eq!(TileId::TreeTrunk as u8, 32);
    }

    #[test]
    fn tile_is_four_bytes_and_roundtrips() {
        assert_eq!(std::mem::size_of::<Tile>(), 4);
        let t = Tile {
            id: TileId::Door,
            wall: WallId::Wood,
            liquid: Liquid::new(LiquidKind::Water, 5),
            state: state::part(0, 2) | state::DOOR_OPEN,
        };
        assert_eq!(Tile::from_bytes(t.to_bytes()), Some(t));
        assert_eq!(Tile::from_bytes(Tile::AIR.to_bytes()), Some(Tile::AIR));
        // Invalid tile id / liquid bytes are rejected.
        assert_eq!(Tile::from_bytes([200, 0, 0, 0]), None);
        assert_eq!(Tile::from_bytes([0, 0, 0x0f, 0]), None);
        assert_eq!(Tile::from_bytes([0, 0, 0x19, 0]), None); // water level 9
    }

    #[test]
    fn door_solidity_follows_state() {
        let mut door = Tile::of(TileId::Door);
        assert!(door.is_solid());
        door.state |= state::DOOR_OPEN;
        assert!(!door.is_solid());
        assert!(Tile::of(TileId::Stone).is_solid());
        assert!(!Tile::of(TileId::Platform).is_solid());
        assert!(Tile::of(TileId::Platform).is_platform());
        assert!(!Tile::of(TileId::TreeTrunk).is_solid());
    }

    #[test]
    fn liquid_packing() {
        let l = Liquid::new(LiquidKind::Lava, 8);
        assert_eq!(l.kind(), Some(LiquidKind::Lava));
        assert_eq!(l.level(), 8);
        assert_eq!(Liquid::from_raw(l.raw()), Some(l));
        assert!(Liquid::NONE.is_none());
        assert_eq!(Liquid::new(LiquidKind::Water, 200).level(), 8);
        assert_eq!(Liquid::from_raw(0x30), None); // kind 3 invalid
        assert_eq!(Liquid::from_raw(0x10), None); // water level 0 invalid
    }

    #[test]
    fn hellstone_and_obsidian_need_power_55() {
        assert_eq!(TileId::Hellstone.data().min_power, 55);
        assert_eq!(TileId::Obsidian.data().min_power, 55);
        assert_eq!(TileId::Hellstone.data().light, 13);
        assert!(!TileId::RitualAltar.data().breakable);
        assert_eq!(TileId::Bed.data().size, (4, 2));
        assert_eq!(TileId::Door.data().size, (1, 3));
    }

    #[test]
    fn multi_tile_part_packing() {
        let s = state::part(3, 1);
        assert_eq!(state::part_x(s), 3);
        assert_eq!(state::part_y(s), 1);
        assert_eq!(state::part_x(s | state::WALL_PLACED), 3);
    }
}
