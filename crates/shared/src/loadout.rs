//! Pure loadout summaries: what a player's worn armor + accessories add up
//! to (DESIGN §4.2 armor/set bonuses, §4.3 accessories).
//!
//! Everything here reads the flat §8 inventory layout (`items::inventory`)
//! and the static tables — no other state — so the server (authoritative
//! application) and the client (prediction/HUD) call the same functions on
//! the same synced slots and always agree.

use crate::items::{
    inventory, set_bonus, AccessoryEffect, ArmorSet, InvSlot, ItemId, SetBonus,
    BAND_OF_VIGOR_REGEN_HPS, BLOODSHOT_LENS_DAMAGE_MULT, OBSIDIAN_CHARM_LAVA_DAMAGE_MULT,
    SKULL_CHARM_DEFENSE, SWIFT_BOOTS_SPEED_MULT,
};
use crate::physics::PhysicsMods;

/// Ember set bonus: +10% melee damage (§4.2).
pub const EMBER_SET_MELEE_MULT: f32 = 1.10;

/// Worn armor pieces (the three armor slots), in slot order Head/Chest/Legs.
fn armor_items(slots: &[Option<InvSlot>]) -> impl Iterator<Item = ItemId> + '_ {
    slots
        .iter()
        .skip(inventory::ARMOR_START)
        .take(inventory::ARMOR)
        .flatten()
        .map(|s| s.item)
}

/// Equipped accessory effects (the three accessory slots). Duplicates can't
/// be equipped (the slot-op engine rejects them), and effects of *different*
/// accessories stack (§4.3) — consumers below treat these as a set.
fn accessory_effects(slots: &[Option<InvSlot>]) -> impl Iterator<Item = AccessoryEffect> + '_ {
    slots
        .iter()
        .skip(inventory::ACCESSORY_START)
        .take(inventory::ACCESSORY)
        .flatten()
        .filter_map(|s| s.item.data().accessory)
}

fn has_effect(slots: &[Option<InvSlot>], e: AccessoryEffect) -> bool {
    accessory_effects(slots).any(|a| a == e)
}

/// The armor set worn on all three slots, if any (§4.2 set bonus condition).
pub fn full_set(slots: &[Option<InvSlot>]) -> Option<ArmorSet> {
    let mut set: Option<ArmorSet> = None;
    for i in 0..inventory::ARMOR {
        let worn = slots.get(inventory::ARMOR_START + i).copied().flatten()?;
        let s = worn.item.data().armor.and_then(|a| a.set)?;
        if *set.get_or_insert(s) != s {
            return None; // pieces from different sets
        }
    }
    set
}

/// Total defense (§4.2): sum of worn piece defense, plus the set bonus when
/// all three pieces of one set are worn, plus the Skull Charm (§4.3).
pub fn defense(slots: &[Option<InvSlot>]) -> u32 {
    let mut total: u32 = armor_items(slots)
        .filter_map(|i| i.data().armor)
        .map(|a| a.defense as u32)
        .sum();
    if let Some(set) = full_set(slots) {
        if let SetBonus::Defense(d) = set_bonus(set) {
            total += d as u32;
        }
    }
    if has_effect(slots, AccessoryEffect::DefenseBoost) {
        total += SKULL_CHARM_DEFENSE as u32;
    }
    total
}

/// Physics modifiers from the equipped accessories (§4.3): Swift Boots run
/// speed, Gust Jar double jump, Lucky Charm fall-damage immunity. Fed into
/// `physics::step_player_with_mods` by both the server and the predicting
/// client.
pub fn physics_mods(slots: &[Option<InvSlot>]) -> PhysicsMods {
    PhysicsMods {
        speed_mult: if has_effect(slots, AccessoryEffect::RunSpeed) {
            SWIFT_BOOTS_SPEED_MULT
        } else {
            1.0
        },
        extra_air_jumps: has_effect(slots, AccessoryEffect::DoubleJump) as u8,
        no_fall_damage: has_effect(slots, AccessoryEffect::NoFallDamage),
    }
}

/// Non-physics equipment effects. Combat/regen/lava systems land in later
/// PRs — these are their hooks; producing them here keeps every §4.2/§4.3
/// number in one place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectMods {
    /// Bonus passive regen, HP/s (Band of Vigor).
    pub regen_bonus_hps: f32,
    /// Burning immunity (Obsidian Charm, or full Ember set §4.2).
    pub burn_immune: bool,
    /// Lava contact damage multiplier (Obsidian Charm halves it).
    pub lava_damage_mult: f32,
    /// All-damage multiplier (Bloodshot Lens).
    pub damage_mult: f32,
    /// Melee-only damage multiplier (Ember set bonus).
    pub melee_damage_mult: f32,
    /// Green/blue slimes never aggro (Royal Gel Charm).
    pub slime_friend: bool,
}

impl Default for EffectMods {
    fn default() -> Self {
        EffectMods {
            regen_bonus_hps: 0.0,
            burn_immune: false,
            lava_damage_mult: 1.0,
            damage_mult: 1.0,
            melee_damage_mult: 1.0,
            slime_friend: false,
        }
    }
}

/// Computes the [`EffectMods`] hooks for a loadout (§4.2 set bonuses, §4.3).
pub fn effect_mods(slots: &[Option<InvSlot>]) -> EffectMods {
    let fire_ward = has_effect(slots, AccessoryEffect::FireWard);
    let ember_set = matches!(full_set(slots), Some(ArmorSet::Ember));
    EffectMods {
        regen_bonus_hps: if has_effect(slots, AccessoryEffect::HpRegen) {
            BAND_OF_VIGOR_REGEN_HPS
        } else {
            0.0
        },
        burn_immune: fire_ward || ember_set,
        lava_damage_mult: if fire_ward {
            OBSIDIAN_CHARM_LAVA_DAMAGE_MULT
        } else {
            1.0
        },
        damage_mult: if has_effect(slots, AccessoryEffect::DamageBoost) {
            BLOODSHOT_LENS_DAMAGE_MULT
        } else {
            1.0
        },
        melee_damage_mult: if ember_set { EMBER_SET_MELEE_MULT } else { 1.0 },
        slime_friend: has_effect(slots, AccessoryEffect::SlimeFriend),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::inventory::{ACCESSORY_START, ARMOR_START, TOTAL};

    fn empty() -> Vec<Option<InvSlot>> {
        vec![None; TOTAL]
    }

    fn with_armor(items: [Option<ItemId>; 3]) -> Vec<Option<InvSlot>> {
        let mut slots = empty();
        for (i, item) in items.into_iter().enumerate() {
            slots[ARMOR_START + i] = item.map(|it| InvSlot::new(it, 1));
        }
        slots
    }

    fn with_accessories(items: &[ItemId]) -> Vec<Option<InvSlot>> {
        let mut slots = empty();
        for (i, &item) in items.iter().enumerate() {
            slots[ACCESSORY_START + i] = Some(InvSlot::new(item, 1));
        }
        slots
    }

    #[test]
    fn defense_per_design_4_2() {
        assert_eq!(defense(&empty()), 0);
        // Full gold set: 4 + 5 + 4 + 3 set bonus = 16.
        let gold = with_armor([
            Some(ItemId::GoldHelmet),
            Some(ItemId::GoldChestplate),
            Some(ItemId::GoldGreaves),
        ]);
        assert_eq!(defense(&gold), 16);
        // Full wood set: 1 + 1 + 0 + 1 = 3.
        let wood = with_armor([
            Some(ItemId::WoodHelmet),
            Some(ItemId::WoodChestplate),
            Some(ItemId::WoodGreaves),
        ]);
        assert_eq!(defense(&wood), 3);
        // Mixed set: no bonus. Gold helm + iron chest + gold greaves = 4+3+4.
        let mixed = with_armor([
            Some(ItemId::GoldHelmet),
            Some(ItemId::IronChestplate),
            Some(ItemId::GoldGreaves),
        ]);
        assert_eq!(defense(&mixed), 11);
        assert_eq!(full_set(&mixed), None);
        // Two pieces of a set: no bonus.
        let partial = with_armor([Some(ItemId::GoldHelmet), Some(ItemId::GoldChestplate), None]);
        assert_eq!(defense(&partial), 9);
        // Mining Helmet alone: 0 defense, off-set.
        let miner = with_armor([Some(ItemId::MiningHelmet), None, None]);
        assert_eq!(defense(&miner), 0);
        // Ember set: 8+9+8, set bonus is EmberFury (no defense).
        let ember = with_armor([
            Some(ItemId::EmberHelmet),
            Some(ItemId::EmberChestplate),
            Some(ItemId::EmberGreaves),
        ]);
        assert_eq!(defense(&ember), 25);
        assert_eq!(full_set(&ember), Some(ArmorSet::Ember));
    }

    #[test]
    fn skull_charm_adds_four_defense() {
        let mut slots = with_armor([
            Some(ItemId::GoldHelmet),
            Some(ItemId::GoldChestplate),
            Some(ItemId::GoldGreaves),
        ]);
        slots[ACCESSORY_START] = Some(InvSlot::new(ItemId::SkullCharm, 1));
        assert_eq!(defense(&slots), 20);
        // A Skull Charm sitting in the backpack does nothing.
        let mut idle = empty();
        idle[inventory::BACKPACK_START] = Some(InvSlot::new(ItemId::SkullCharm, 1));
        assert_eq!(defense(&idle), 0);
    }

    #[test]
    fn physics_mods_per_accessory() {
        assert_eq!(physics_mods(&empty()), PhysicsMods::NONE);
        let boots = physics_mods(&with_accessories(&[ItemId::SwiftBoots]));
        assert_eq!(boots.speed_mult, SWIFT_BOOTS_SPEED_MULT);
        assert_eq!(boots.extra_air_jumps, 0);
        assert!(!boots.no_fall_damage);

        let jar = physics_mods(&with_accessories(&[ItemId::GustJar]));
        assert_eq!(jar.extra_air_jumps, 1);
        assert_eq!(jar.speed_mult, 1.0);

        let charm = physics_mods(&with_accessories(&[ItemId::LuckyCharm]));
        assert!(charm.no_fall_damage);

        let all = physics_mods(&with_accessories(&[
            ItemId::SwiftBoots,
            ItemId::GustJar,
            ItemId::LuckyCharm,
        ]));
        assert_eq!(
            all,
            PhysicsMods {
                speed_mult: SWIFT_BOOTS_SPEED_MULT,
                extra_air_jumps: 1,
                no_fall_damage: true,
            }
        );

        // Accessories in the backpack don't count.
        let mut idle = empty();
        idle[0] = Some(InvSlot::new(ItemId::SwiftBoots, 1));
        assert_eq!(physics_mods(&idle), PhysicsMods::NONE);
    }

    #[test]
    fn effect_mod_hooks() {
        assert_eq!(effect_mods(&empty()), EffectMods::default());
        let e = effect_mods(&with_accessories(&[
            ItemId::BandOfVigor,
            ItemId::ObsidianCharm,
            ItemId::BloodshotLens,
        ]));
        assert_eq!(e.regen_bonus_hps, BAND_OF_VIGOR_REGEN_HPS);
        assert!(e.burn_immune);
        assert_eq!(e.lava_damage_mult, OBSIDIAN_CHARM_LAVA_DAMAGE_MULT);
        assert_eq!(e.damage_mult, BLOODSHOT_LENS_DAMAGE_MULT);
        assert!(!e.slime_friend);

        let royal = effect_mods(&with_accessories(&[ItemId::RoyalGelCharm]));
        assert!(royal.slime_friend);

        // Ember set: burn immunity + melee damage without any accessory.
        let ember = with_armor([
            Some(ItemId::EmberHelmet),
            Some(ItemId::EmberChestplate),
            Some(ItemId::EmberGreaves),
        ]);
        let e = effect_mods(&ember);
        assert!(e.burn_immune);
        assert_eq!(e.melee_damage_mult, EMBER_SET_MELEE_MULT);
        assert_eq!(e.lava_damage_mult, 1.0, "set bonus does not halve lava");
    }
}
