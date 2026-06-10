//! Item definitions and the static [`ITEM_DATA`] table (DESIGN §4).
//!
//! One row per item; numbers come straight from DESIGN.md §4 (tools, armor,
//! accessories, recipes) and §7.3 (merchant prices, which pin the `value` of
//! the eight shop items; other values are tier-scaled estimates — the
//! merchant buys anything back at 20% of `value`).

use serde::{Deserialize, Serialize};

use crate::macros::id_table;
use crate::tiles::{TileId, ToolKind, WallId};

// ---- Stack-size rules (§0) -------------------------------------------------

pub const STACK_BLOCK: u16 = 999; // blocks / materials / ammo
pub const STACK_POTION: u16 = 30;
pub const STACK_COIN: u16 = 999;
pub const STACK_ONE: u16 = 1; // tools / weapons / armor / accessories

// ---- Combat / item-behavior constants (§4) ---------------------------------

/// Arrows fly at 35 t/s under 0.35× gravity, despawn after 5 s, and have a
/// 50% chance to be recoverable from terrain (§4.1).
pub const ARROW_SPEED: f32 = 35.0;
pub const ARROW_GRAVITY_MULT: f32 = 0.35;
pub const ARROW_LIFETIME_SECS: f32 = 5.0;
pub const ARROW_RECOVER_CHANCE: f32 = 0.5;

/// Melee hitbox: 3×3 tile arc in facing direction for the swing duration.
pub const MELEE_ARC_TILES: f32 = 3.0;
/// Pickaxes (and hammers) knock back at 2 t/s, swords at 5 t/s.
pub const PICK_KNOCKBACK: f32 = 2.0;
pub const SWORD_KNOCKBACK: f32 = 5.0;

/// Ember Blade: 10% chance to inflict Burning 3 s. Flaming arrows: 33%.
pub const EMBER_BLADE_BURN_CHANCE: f32 = 0.10;
pub const EMBER_BLADE_BURN_SECS: f32 = 3.0;
pub const FLAMING_ARROW_BURN_CHANCE: f32 = 0.33;
pub const FLAMING_ARROW_BURN_SECS: f32 = 3.0;

/// Healing potions inflict Potion Sickness for 60 s (§4.4).
pub const POTION_SICKNESS_SECS: f32 = 60.0;

/// Warp Mirror channels for 1 s before teleporting (§4.3).
pub const WARP_MIRROR_CHANNEL_SECS: f32 = 1.0;

// ---- Accessory effect magnitudes (§4.3) ------------------------------------

pub const SWIFT_BOOTS_SPEED_MULT: f32 = 1.25;
pub const GUST_JAR_SECOND_JUMP_MULT: f32 = 0.75;
pub const BAND_OF_VIGOR_REGEN_HPS: f32 = 0.5;
pub const OBSIDIAN_CHARM_LAVA_DAMAGE_MULT: f32 = 0.5;
pub const BLOODSHOT_LENS_DAMAGE_MULT: f32 = 1.10;
pub const SKULL_CHARM_DEFENSE: u8 = 4;

/// Where an item goes when placed in the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Placement {
    Tile(TileId),
    Wall(WallId),
}

/// Mining/chopping/hammering stats. Tools also swing as (weak) melee
/// weapons via their `weapon` row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolStats {
    pub kind: ToolKind,
    pub power: u8,
    pub use_secs: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WeaponKind {
    Melee,
    Bow,
    /// Ammo; damage adds to the firing bow's damage (§4.1).
    Arrow,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeaponStats {
    pub kind: WeaponKind,
    pub damage: u16,
    pub use_secs: f32,
    /// Knockback in tiles/s.
    pub knockback: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArmorSlot {
    Head,
    Chest,
    Legs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArmorSet {
    Wood,
    Copper,
    Iron,
    Silver,
    Gold,
    Ember,
}

/// Set bonus applied when all three pieces of a set are worn (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetBonus {
    Defense(u8),
    /// Ember set: +10% melee damage and immunity to Burning.
    EmberFury,
}

pub fn set_bonus(set: ArmorSet) -> SetBonus {
    match set {
        ArmorSet::Wood => SetBonus::Defense(1),
        ArmorSet::Copper | ArmorSet::Iron => SetBonus::Defense(2),
        ArmorSet::Silver | ArmorSet::Gold => SetBonus::Defense(3),
        ArmorSet::Ember => SetBonus::EmberFury,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmorStats {
    pub slot: ArmorSlot,
    pub defense: u8,
    /// `None` for off-set head gear (Mining Helmet).
    pub set: Option<ArmorSet>,
}

/// Effect tag for the 8 accessories (§4.3). Effects stack across different
/// accessories; duplicates don't stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessoryEffect {
    /// Swift Boots: +25% max run speed.
    RunSpeed,
    /// Gust Jar: double jump at 75% height; mid-air jump negates fall damage.
    DoubleJump,
    /// Lucky Charm: no fall damage.
    NoFallDamage,
    /// Band of Vigor: +0.5 HP/s passive regen.
    HpRegen,
    /// Obsidian Charm: immune to Burning; lava deals half damage.
    FireWard,
    /// Royal Gel Charm: green/blue slimes never aggro.
    SlimeFriend,
    /// Bloodshot Lens: +10% all damage.
    DamageBoost,
    /// Skull Charm: +4 defense.
    DefenseBoost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BossKind {
    SlimeMonarch,
    Watcher,
    BoneWarden,
}

/// What using/consuming the item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consumable {
    /// Restore HP (and inflict Potion Sickness).
    Heal(u16),
    /// Life Crystal: +20 max HP up to 400.
    MaxHpUp(u16),
    /// Boss summon item (consumed on use; Ritual Altar crafts them).
    SummonBoss(BossKind),
    /// Warp Mirror: 1 s channel, teleport to spawn. Not consumed.
    TeleportToSpawn,
}

/// Static per-item data; one row per [`ItemId`] in [`ITEM_DATA`].
#[derive(Debug, Clone, PartialEq)]
pub struct ItemData {
    pub name: &'static str,
    pub max_stack: u16,
    /// Base value in copper coins. Merchant sale price; buy-back is 20%.
    pub value: u32,
    pub places: Option<Placement>,
    pub tool: Option<ToolStats>,
    pub weapon: Option<WeaponStats>,
    pub armor: Option<ArmorStats>,
    pub accessory: Option<AccessoryEffect>,
    pub consumable: Option<Consumable>,
}

impl ItemData {
    const fn new(name: &'static str, max_stack: u16, value: u32) -> Self {
        ItemData {
            name,
            max_stack,
            value,
            places: None,
            tool: None,
            weapon: None,
            armor: None,
            accessory: None,
            consumable: None,
        }
    }

    /// A block item: stacks to 999 and places `tile`.
    const fn block(name: &'static str, value: u32, tile: TileId) -> Self {
        ItemData::new(name, STACK_BLOCK, value).tile(tile)
    }

    /// A plain crafting material: stacks to 999.
    const fn material(name: &'static str, value: u32) -> Self {
        ItemData::new(name, STACK_BLOCK, value)
    }

    const fn tile(mut self, t: TileId) -> Self {
        self.places = Some(Placement::Tile(t));
        self
    }
    const fn wall(mut self, w: WallId) -> Self {
        self.places = Some(Placement::Wall(w));
        self
    }
    const fn tool(mut self, kind: ToolKind, power: u8, use_secs: f32) -> Self {
        self.tool = Some(ToolStats {
            kind,
            power,
            use_secs,
        });
        self
    }
    const fn weapon(
        mut self,
        kind: WeaponKind,
        damage: u16,
        use_secs: f32,
        knockback: f32,
    ) -> Self {
        self.weapon = Some(WeaponStats {
            kind,
            damage,
            use_secs,
            knockback,
        });
        self
    }
    const fn melee(self, damage: u16, use_secs: f32, knockback: f32) -> Self {
        self.weapon(WeaponKind::Melee, damage, use_secs, knockback)
    }
    const fn armor(mut self, slot: ArmorSlot, defense: u8, set: Option<ArmorSet>) -> Self {
        self.armor = Some(ArmorStats { slot, defense, set });
        self
    }
    const fn accessory(mut self, e: AccessoryEffect) -> Self {
        self.accessory = Some(e);
        self
    }
    const fn consumable(mut self, c: Consumable) -> Self {
        self.consumable = Some(c);
        self
    }
}

use ArmorSet as Set;
use ArmorSlot::{Chest as ChestSlot, Head, Legs};
use Consumable as Use;

id_table! {
    /// Every item in the game (DESIGN §4). `u16` repr for headroom; postcard
    /// varint-encodes ids so small ones still cost one byte on the wire.
    pub enum ItemId(u16), pub table ITEM_DATA: ItemData {
        // ---- Blocks (place their tile) ------------------------------------
        Dirt => ItemData::block("Dirt", 0, TileId::Dirt),
        Stone => ItemData::block("Stone", 0, TileId::Stone),
        Sand => ItemData::block("Sand", 0, TileId::Sand),
        Clay => ItemData::block("Clay", 0, TileId::Clay),
        WoodPlank => ItemData::block("Wood Plank", 5, TileId::WoodPlank),
        Ash => ItemData::block("Ash", 0, TileId::Ash),
        StoneBrick => ItemData::block("Stone Brick", 5, TileId::StoneBrick),
        EmberBrick => ItemData::block("Ember Brick", 10, TileId::EmberBrick),
        CopperOre => ItemData::block("Copper Ore", 50, TileId::CopperOre),
        IronOre => ItemData::block("Iron Ore", 75, TileId::IronOre),
        SilverOre => ItemData::block("Silver Ore", 150, TileId::SilverOre),
        GoldOre => ItemData::block("Gold Ore", 300, TileId::GoldOre),
        Hellstone => ItemData::block("Hellstone", 400, TileId::Hellstone),
        Obsidian => ItemData::block("Obsidian", 100, TileId::Obsidian),

        // ---- Walls ---------------------------------------------------------
        DirtWall => ItemData::material("Dirt Wall", 2).wall(WallId::Dirt),
        StoneWall => ItemData::material("Stone Wall", 2).wall(WallId::Stone),
        WoodWall => ItemData::material("Wood Wall", 2).wall(WallId::Wood),

        // ---- Furniture & placeables ----------------------------------------
        Torch => ItemData::block("Torch", 50, TileId::Torch),
        Platform => ItemData::block("Platform", 4, TileId::Platform),
        Door => ItemData::block("Door", 200, TileId::Door),
        Chest => ItemData::block("Chest", 500, TileId::Chest),
        Workbench => ItemData::block("Workbench", 150, TileId::Workbench),
        Furnace => ItemData::block("Furnace", 300, TileId::Furnace),
        Anvil => ItemData::block("Anvil", 5_000, TileId::Anvil),
        InfernalForge => ItemData::block("Infernal Forge", 20_000, TileId::InfernalForge),
        Table => ItemData::block("Table", 300, TileId::Table),
        Chair => ItemData::block("Chair", 150, TileId::Chair),
        Bed => ItemData::block("Bed", 1_000, TileId::Bed),

        // ---- Materials -------------------------------------------------------
        Wood => ItemData::material("Wood", 10),
        Gel => ItemData::material("Gel", 5),
        Lens => ItemData::material("Lens", 100),
        Bone => ItemData::material("Bone", 30),
        Cobweb => ItemData::material("Cobweb", 5),
        /// Plants a sapling on grass (§4.3 usable special).
        Acorn => ItemData::material("Acorn", 10).tile(TileId::Sapling),
        Mushroom => ItemData::material("Mushroom", 25),
        Glass => ItemData::material("Glass", 10),
        /// Place on a Table/Workbench to enable potion crafting (§4.4);
        /// stored as `state::BOTTLE_ON_TOP` on that tile.
        Bottle => ItemData::material("Bottle", 20),

        // ---- Bars ------------------------------------------------------------
        CopperBar => ItemData::material("Copper Bar", 150),
        IronBar => ItemData::material("Iron Bar", 225),
        SilverBar => ItemData::material("Silver Bar", 600),
        GoldBar => ItemData::material("Gold Bar", 1_200),
        EmberBar => ItemData::material("Ember Bar", 1_500),

        // ---- Coins (value = denomination in copper, §0) ------------------------
        CopperCoin => ItemData::new("Copper Coin", STACK_COIN, 1),
        SilverCoin => ItemData::new("Silver Coin", STACK_COIN, 100),
        GoldCoin => ItemData::new("Gold Coin", STACK_COIN, 10_000),
        PlatinumCoin => ItemData::new("Platinum Coin", STACK_COIN, 1_000_000),

        // ---- Pickaxes (power/use §4.1; melee damage 4/5/6/7/8/12, kb 2) -------
        WoodPickaxe => ItemData::new("Wood Pickaxe", STACK_ONE, 200)
            .tool(ToolKind::Pick, 25, 0.30).melee(4, 0.30, PICK_KNOCKBACK),
        CopperPickaxe => ItemData::new("Copper Pickaxe", STACK_ONE, 500)
            .tool(ToolKind::Pick, 35, 0.25).melee(5, 0.25, PICK_KNOCKBACK),
        IronPickaxe => ItemData::new("Iron Pickaxe", STACK_ONE, 1_000)
            .tool(ToolKind::Pick, 40, 0.23).melee(6, 0.23, PICK_KNOCKBACK),
        SilverPickaxe => ItemData::new("Silver Pickaxe", STACK_ONE, 2_000)
            .tool(ToolKind::Pick, 45, 0.22).melee(7, 0.22, PICK_KNOCKBACK),
        GoldPickaxe => ItemData::new("Gold Pickaxe", STACK_ONE, 4_000)
            .tool(ToolKind::Pick, 55, 0.20).melee(8, 0.20, PICK_KNOCKBACK),
        EmberPickaxe => ItemData::new("Ember Pickaxe", STACK_ONE, 10_000)
            .tool(ToolKind::Pick, 100, 0.17).melee(12, 0.17, PICK_KNOCKBACK),

        // ---- Axes (power §4.1; use mirrors pick tiers; dmg 5–14 by tier) ------
        WoodAxe => ItemData::new("Wood Axe", STACK_ONE, 150)
            .tool(ToolKind::Axe, 25, 0.30).melee(5, 0.30, PICK_KNOCKBACK),
        CopperAxe => ItemData::new("Copper Axe", STACK_ONE, 400)
            .tool(ToolKind::Axe, 35, 0.25).melee(7, 0.25, PICK_KNOCKBACK),
        IronAxe => ItemData::new("Iron Axe", STACK_ONE, 800)
            .tool(ToolKind::Axe, 40, 0.23).melee(8, 0.23, PICK_KNOCKBACK),
        SilverAxe => ItemData::new("Silver Axe", STACK_ONE, 1_600)
            .tool(ToolKind::Axe, 45, 0.22).melee(10, 0.22, PICK_KNOCKBACK),
        GoldAxe => ItemData::new("Gold Axe", STACK_ONE, 3_200)
            .tool(ToolKind::Axe, 55, 0.20).melee(12, 0.20, PICK_KNOCKBACK),
        EmberAxe => ItemData::new("Ember Axe", STACK_ONE, 8_000)
            .tool(ToolKind::Axe, 100, 0.17).melee(14, 0.17, PICK_KNOCKBACK),

        // ---- Hammers (wall removal, §4.1) --------------------------------------
        WoodHammer => ItemData::new("Wood Hammer", STACK_ONE, 150)
            .tool(ToolKind::Hammer, 25, 0.33).melee(4, 0.33, PICK_KNOCKBACK),
        IronHammer => ItemData::new("Iron Hammer", STACK_ONE, 800)
            .tool(ToolKind::Hammer, 55, 0.28).melee(9, 0.28, PICK_KNOCKBACK),

        // ---- Swords (dmg/use §4.1, kb 5) ----------------------------------------
        WoodSword => ItemData::new("Wood Sword", STACK_ONE, 150)
            .melee(7, 0.42, SWORD_KNOCKBACK),
        CopperSword => ItemData::new("Copper Sword", STACK_ONE, 400)
            .melee(9, 0.40, SWORD_KNOCKBACK),
        IronSword => ItemData::new("Iron Sword", STACK_ONE, 800)
            .melee(12, 0.38, SWORD_KNOCKBACK),
        SilverSword => ItemData::new("Silver Sword", STACK_ONE, 1_600)
            .melee(14, 0.37, SWORD_KNOCKBACK),
        GoldSword => ItemData::new("Gold Sword", STACK_ONE, 3_200)
            .melee(16, 0.35, SWORD_KNOCKBACK),
        /// 10% chance to inflict Burning 3 s ([`EMBER_BLADE_BURN_CHANCE`]).
        EmberBlade => ItemData::new("Ember Blade", STACK_ONE, 10_000)
            .melee(36, 0.55, SWORD_KNOCKBACK),
        /// Zombie drop (§5.1): a 10-damage sword.
        ZombieArm => ItemData::new("Zombie Arm", STACK_ONE, 500)
            .melee(10, 0.40, SWORD_KNOCKBACK),
        /// Ash Demon drop (§5.1). DESIGN gives no player-side stats; modeled
        /// as a 30-damage melee weapon matching its projectile damage.
        VoidSickle => ItemData::new("Void Sickle", STACK_ONE, 15_000)
            .melee(30, 0.45, SWORD_KNOCKBACK),

        // ---- Bows & arrows (§4.1) -----------------------------------------------
        WoodenBow => ItemData::new("Wooden Bow", STACK_ONE, 200)
            .weapon(WeaponKind::Bow, 4, 0.50, PICK_KNOCKBACK),
        IronBow => ItemData::new("Iron Bow", STACK_ONE, 800)
            .weapon(WeaponKind::Bow, 8, 0.47, PICK_KNOCKBACK),
        GoldBow => ItemData::new("Gold Bow", STACK_ONE, 3_200)
            .weapon(WeaponKind::Bow, 11, 0.45, PICK_KNOCKBACK),
        /// Wooden arrows fired become flaming arrows.
        Cinderbow => ItemData::new("Cinderbow", STACK_ONE, 10_000)
            .weapon(WeaponKind::Bow, 29, 0.40, PICK_KNOCKBACK),
        WoodenArrow => ItemData::new("Wooden Arrow", STACK_BLOCK, 5)
            .weapon(WeaponKind::Arrow, 5, 0.0, 0.0),
        /// 33% chance of Burning 3 s ([`FLAMING_ARROW_BURN_CHANCE`]).
        FlamingArrow => ItemData::new("Flaming Arrow", STACK_BLOCK, 10)
            .weapon(WeaponKind::Arrow, 7, 0.0, 0.0),

        // ---- Armor (defense per §4.2) ---------------------------------------------
        WoodHelmet => ItemData::new("Wood Helmet", STACK_ONE, 50)
            .armor(Head, 1, Some(Set::Wood)),
        WoodChestplate => ItemData::new("Wood Chestplate", STACK_ONE, 50)
            .armor(ChestSlot, 1, Some(Set::Wood)),
        WoodGreaves => ItemData::new("Wood Greaves", STACK_ONE, 50)
            .armor(Legs, 0, Some(Set::Wood)),
        CopperHelmet => ItemData::new("Copper Helmet", STACK_ONE, 300)
            .armor(Head, 1, Some(Set::Copper)),
        CopperChestplate => ItemData::new("Copper Chestplate", STACK_ONE, 500)
            .armor(ChestSlot, 2, Some(Set::Copper)),
        CopperGreaves => ItemData::new("Copper Greaves", STACK_ONE, 400)
            .armor(Legs, 1, Some(Set::Copper)),
        IronHelmet => ItemData::new("Iron Helmet", STACK_ONE, 600)
            .armor(Head, 2, Some(Set::Iron)),
        IronChestplate => ItemData::new("Iron Chestplate", STACK_ONE, 1_000)
            .armor(ChestSlot, 3, Some(Set::Iron)),
        IronGreaves => ItemData::new("Iron Greaves", STACK_ONE, 800)
            .armor(Legs, 2, Some(Set::Iron)),
        SilverHelmet => ItemData::new("Silver Helmet", STACK_ONE, 1_200)
            .armor(Head, 3, Some(Set::Silver)),
        SilverChestplate => ItemData::new("Silver Chestplate", STACK_ONE, 2_000)
            .armor(ChestSlot, 4, Some(Set::Silver)),
        SilverGreaves => ItemData::new("Silver Greaves", STACK_ONE, 1_600)
            .armor(Legs, 3, Some(Set::Silver)),
        GoldHelmet => ItemData::new("Gold Helmet", STACK_ONE, 2_400)
            .armor(Head, 4, Some(Set::Gold)),
        GoldChestplate => ItemData::new("Gold Chestplate", STACK_ONE, 4_000)
            .armor(ChestSlot, 5, Some(Set::Gold)),
        GoldGreaves => ItemData::new("Gold Greaves", STACK_ONE, 3_200)
            .armor(Legs, 4, Some(Set::Gold)),
        EmberHelmet => ItemData::new("Ember Helmet", STACK_ONE, 6_000)
            .armor(Head, 8, Some(Set::Ember)),
        EmberChestplate => ItemData::new("Ember Chestplate", STACK_ONE, 10_000)
            .armor(ChestSlot, 9, Some(Set::Ember)),
        EmberGreaves => ItemData::new("Ember Greaves", STACK_ONE, 8_000)
            .armor(Legs, 8, Some(Set::Ember)),
        /// 0 defense; emits light 20 at the player (§4.3).
        MiningHelmet => ItemData::new("Mining Helmet", STACK_ONE, 40_000)
            .armor(Head, 0, None),

        // ---- Accessories (§4.3) -------------------------------------------------
        SwiftBoots => ItemData::new("Swift Boots", STACK_ONE, 20_000)
            .accessory(AccessoryEffect::RunSpeed),
        GustJar => ItemData::new("Gust Jar", STACK_ONE, 20_000)
            .accessory(AccessoryEffect::DoubleJump),
        LuckyCharm => ItemData::new("Lucky Charm", STACK_ONE, 20_000)
            .accessory(AccessoryEffect::NoFallDamage),
        BandOfVigor => ItemData::new("Band of Vigor", STACK_ONE, 20_000)
            .accessory(AccessoryEffect::HpRegen),
        ObsidianCharm => ItemData::new("Obsidian Charm", STACK_ONE, 30_000)
            .accessory(AccessoryEffect::FireWard),
        RoyalGelCharm => ItemData::new("Royal Gel Charm", STACK_ONE, 50_000)
            .accessory(AccessoryEffect::SlimeFriend),
        BloodshotLens => ItemData::new("Bloodshot Lens", STACK_ONE, 50_000)
            .accessory(AccessoryEffect::DamageBoost),
        SkullCharm => ItemData::new("Skull Charm", STACK_ONE, 50_000)
            .accessory(AccessoryEffect::DefenseBoost),

        // ---- Usables & consumables ------------------------------------------------
        WarpMirror => ItemData::new("Warp Mirror", STACK_ONE, 25_000)
            .consumable(Use::TeleportToSpawn),
        LesserHealingPotion => ItemData::new("Lesser Healing Potion", STACK_POTION, 300)
            .consumable(Use::Heal(50)),
        HealingPotion => ItemData::new("Healing Potion", STACK_POTION, 1_000)
            .consumable(Use::Heal(100)),
        LifeCrystal => ItemData::material("Life Crystal", 10_000)
            .consumable(Use::MaxHpUp(20)),

        // ---- Crowns & boss summons (§4.4 recipes 67–70) -----------------------------
        GoldCrown => ItemData::material("Gold Crown", 6_000),
        GelCrown => ItemData::material("Gel Crown", 10_000)
            .consumable(Use::SummonBoss(BossKind::SlimeMonarch)),
        /// Night only (server enforces).
        WatchersIris => ItemData::material("Watcher's Iris", 10_000)
            .consumable(Use::SummonBoss(BossKind::Watcher)),
        /// Night only (server enforces).
        CursedEffigy => ItemData::material("Cursed Effigy", 20_000)
            .consumable(Use::SummonBoss(BossKind::BoneWarden)),
    }
}

impl ItemId {
    #[inline]
    pub fn is_placeable(self) -> bool {
        self.data().places.is_some()
    }

    /// Stack-merge limit for this item.
    #[inline]
    pub fn max_stack(self) -> u16 {
        self.data().max_stack
    }
}

/// One occupied inventory/chest slot. Empty slots are `Option::None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvSlot {
    pub item: ItemId,
    pub count: u16,
}

impl InvSlot {
    pub const fn new(item: ItemId, count: u16) -> Self {
        InvSlot { item, count }
    }
}

/// Inventory layout (§8): hotbar 10 + backpack 40 + armor 3 + accessory 3 +
/// trash 1 = 57 slots. Index helpers for the flat slot array.
pub mod inventory {
    pub const HOTBAR: usize = 10;
    pub const BACKPACK: usize = 40;
    pub const ARMOR: usize = 3;
    pub const ACCESSORY: usize = 3;
    /// First index of each region in the flat slot array.
    pub const BACKPACK_START: usize = HOTBAR;
    pub const ARMOR_START: usize = HOTBAR + BACKPACK;
    pub const ACCESSORY_START: usize = ARMOR_START + ARMOR;
    pub const TRASH: usize = ACCESSORY_START + ACCESSORY;
    pub const TOTAL: usize = TRASH + 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_every_item() {
        assert_eq!(ITEM_DATA.len(), ItemId::COUNT);
        for id in ItemId::ALL {
            assert!(!id.data().name.is_empty(), "{id:?} has no name");
        }
    }

    #[test]
    fn stack_rules() {
        assert_eq!(ItemId::Dirt.max_stack(), 999);
        assert_eq!(ItemId::WoodenArrow.max_stack(), 999);
        assert_eq!(ItemId::LesserHealingPotion.max_stack(), 30);
        assert_eq!(ItemId::GoldCoin.max_stack(), 999);
        assert_eq!(ItemId::GoldSword.max_stack(), 1);
        assert_eq!(ItemId::EmberHelmet.max_stack(), 1);
        assert_eq!(ItemId::SwiftBoots.max_stack(), 1);
    }

    #[test]
    fn merchant_shop_prices_pin_values() {
        // §7.3: the merchant's stock prices ARE these items' base values.
        assert_eq!(ItemId::Torch.data().value, 50);
        assert_eq!(ItemId::Bottle.data().value, 20);
        assert_eq!(ItemId::WoodenArrow.data().value, 5);
        assert_eq!(ItemId::LesserHealingPotion.data().value, 300);
        assert_eq!(ItemId::CopperPickaxe.data().value, 500);
        assert_eq!(ItemId::CopperAxe.data().value, 400);
        assert_eq!(ItemId::Anvil.data().value, 5_000);
        assert_eq!(ItemId::MiningHelmet.data().value, 40_000);
    }

    #[test]
    fn coin_values_are_denominations() {
        assert_eq!(ItemId::CopperCoin.data().value, 1);
        assert_eq!(ItemId::SilverCoin.data().value, 100);
        assert_eq!(ItemId::GoldCoin.data().value, 10_000);
        assert_eq!(ItemId::PlatinumCoin.data().value, 1_000_000);
    }

    #[test]
    fn armor_roster() {
        let armor: Vec<_> = ItemId::ALL
            .iter()
            .filter(|i| i.data().armor.is_some())
            .collect();
        assert_eq!(armor.len(), 19); // 6 sets × 3 pieces + Mining Helmet
        let in_sets = armor
            .iter()
            .filter(|i| i.data().armor.expect("armor").set.is_some())
            .count();
        assert_eq!(in_sets, 18);
        // Spot-check §4.2 defense values.
        assert_eq!(
            ItemId::GoldChestplate.data().armor.expect("armor").defense,
            5
        );
        assert_eq!(ItemId::EmberHelmet.data().armor.expect("armor").defense, 8);
        assert_eq!(ItemId::WoodGreaves.data().armor.expect("armor").defense, 0);
        assert_eq!(ItemId::MiningHelmet.data().armor.expect("armor").defense, 0);
    }

    #[test]
    fn accessory_roster_is_eight() {
        let n = ItemId::ALL
            .iter()
            .filter(|i| i.data().accessory.is_some())
            .count();
        assert_eq!(n, 8);
    }

    #[test]
    fn tool_tiers_match_design() {
        let pick = ItemId::GoldPickaxe.data().tool.expect("tool");
        assert_eq!(pick.power, 55); // first pick that mines Hellstone (min 55)
        assert_eq!(ItemId::EmberPickaxe.data().tool.expect("tool").power, 100);
        assert_eq!(ItemId::WoodPickaxe.data().tool.expect("tool").power, 25);
        let blade = ItemId::EmberBlade.data().weapon.expect("weapon");
        assert_eq!(blade.damage, 36);
        let bow = ItemId::Cinderbow.data().weapon.expect("weapon");
        assert_eq!((bow.damage, bow.kind), (29, WeaponKind::Bow));
        assert_eq!(ItemId::WoodenArrow.data().weapon.expect("weapon").damage, 5);
    }

    #[test]
    fn placements_reference_matching_tiles() {
        // Every tile-placing item should be what that tile drops (except
        // Acorn -> Sapling, which drops nothing by design).
        for id in ItemId::ALL {
            if let Some(Placement::Tile(t)) = id.data().places {
                if *id == ItemId::Acorn {
                    assert_eq!(t, TileId::Sapling);
                    continue;
                }
                let drops = t.data().drops.expect("placeable tile must drop");
                assert_eq!(drops.0, *id, "{id:?} places {t:?} which drops {drops:?}");
            }
        }
    }
}
