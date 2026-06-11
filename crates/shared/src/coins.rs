//! Coin math over the inventory engine (DESIGN §0 denominations): counting,
//! paying, and granting copper-denominated amounts across the four coin
//! items, with automatic change-making.
//!
//! All functions work on the flat §8 inventory layout and touch only the
//! carry slots (hotbar + backpack — never armor/accessories/trash), like
//! [`crate::items::add_to_inventory`]. Shared so the server (authoritative
//! transactions) and the client (price affordability previews) agree.

use crate::items::{add_to_inventory, inventory, InvSlot, ItemId};
use crate::{COPPER_PER_GOLD, COPPER_PER_PLATINUM, COPPER_PER_SILVER};

/// Coin denominations, largest first, with their copper value (§0).
pub const COIN_DENOMS: [(ItemId, u32); 4] = [
    (ItemId::PlatinumCoin, COPPER_PER_PLATINUM),
    (ItemId::GoldCoin, COPPER_PER_GOLD),
    (ItemId::SilverCoin, COPPER_PER_SILVER),
    (ItemId::CopperCoin, 1),
];

pub fn is_coin(item: ItemId) -> bool {
    COIN_DENOMS.iter().any(|&(c, _)| c == item)
}

/// Copper value of one coin of `item` (0 for non-coins).
pub fn coin_value(item: ItemId) -> u32 {
    COIN_DENOMS
        .iter()
        .find(|&&(c, _)| c == item)
        .map(|&(_, v)| v)
        .unwrap_or(0)
}

/// Total carried coin value in copper (hotbar + backpack only — coins in
/// armor slots can't exist and trash coins don't count as wealth).
pub fn coin_total(slots: &[Option<InvSlot>]) -> u64 {
    slots
        .iter()
        .take(inventory::ARMOR_START.min(slots.len()))
        .flatten()
        .map(|s| coin_value(s.item) as u64 * s.count as u64)
        .sum()
}

/// Adds `amount` copper worth of coins in canonical denominations (largest
/// first). Returns the indices of every slot changed plus whatever stacks
/// found no room — the caller spills those as world item drops.
pub fn add_coins(slots: &mut [Option<InvSlot>], amount: u64) -> (Vec<usize>, Vec<InvSlot>) {
    let mut changed = Vec::new();
    let mut overflow = Vec::new();
    let mut left = amount;
    for (item, value) in COIN_DENOMS {
        let mut n = left / value as u64;
        left %= value as u64;
        while n > 0 {
            let batch = n.min(item.max_stack() as u64) as u16;
            let (added, idxs) = add_to_inventory(slots, item, batch);
            changed.extend(idxs);
            if added < batch {
                overflow.push(InvSlot::new(item, batch - added));
            }
            n -= batch as u64;
        }
    }
    changed.sort_unstable();
    changed.dedup();
    (changed, overflow)
}

/// Removes exactly `amount` copper worth of coins, breaking larger
/// denominations into change as needed. On success returns the changed slot
/// indices and any change that found no room (caller drops it); returns
/// `None` (touching nothing) when the carried total is insufficient.
///
/// Strategy: drain coin stacks smallest-denomination-first until the amount
/// is covered, then re-add the overshoot as change through [`add_coins`].
pub fn remove_coins(
    slots: &mut [Option<InvSlot>],
    amount: u64,
) -> Option<(Vec<usize>, Vec<InvSlot>)> {
    if coin_total(slots) < amount {
        return None;
    }
    let mut changed = Vec::new();
    let mut taken: u64 = 0;
    let carry = inventory::ARMOR_START.min(slots.len());
    // Smallest denominations first so change-making is minimized.
    for &(item, value) in COIN_DENOMS.iter().rev() {
        for (i, slot) in slots.iter_mut().enumerate().take(carry) {
            if taken >= amount {
                break;
            }
            let Some(s) = slot else { continue };
            if s.item != item {
                continue;
            }
            let need = amount - taken;
            // Coins of this stack needed to cover the rest, rounded up.
            let want = need.div_ceil(value as u64);
            let take = want.min(s.count as u64) as u16;
            taken += take as u64 * value as u64;
            s.count -= take;
            if s.count == 0 {
                *slot = None;
            }
            changed.push(i);
        }
        if taken >= amount {
            break;
        }
    }
    debug_assert!(taken >= amount, "total check guaranteed coverage");
    let mut overflow = Vec::new();
    if taken > amount {
        let (idxs, spill) = add_coins(slots, taken - amount);
        changed.extend(idxs);
        overflow = spill;
    }
    changed.sort_unstable();
    changed.dedup();
    Some((changed, overflow))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::inventory::{ARMOR_START, TOTAL, TRASH};

    fn empty() -> Vec<Option<InvSlot>> {
        vec![None; TOTAL]
    }

    fn count(inv: &[Option<InvSlot>], item: ItemId) -> u32 {
        inv.iter()
            .flatten()
            .filter(|s| s.item == item)
            .map(|s| s.count as u32)
            .sum()
    }

    #[test]
    fn totals_count_denominations_in_carry_slots_only() {
        let mut inv = empty();
        inv[0] = Some(InvSlot::new(ItemId::SilverCoin, 50));
        inv[11] = Some(InvSlot::new(ItemId::CopperCoin, 34));
        inv[12] = Some(InvSlot::new(ItemId::GoldCoin, 2));
        inv[13] = Some(InvSlot::new(ItemId::GoldBar, 5)); // not a coin
        inv[TRASH] = Some(InvSlot::new(ItemId::PlatinumCoin, 1)); // trash ignored
        assert_eq!(coin_total(&inv), 50 * 100 + 34 + 2 * 10_000);
    }

    #[test]
    fn add_coins_uses_canonical_denominations() {
        let mut inv = empty();
        let (changed, overflow) = add_coins(&mut inv, 1_020_304);
        assert!(overflow.is_empty());
        assert_eq!(changed, vec![0, 1, 2, 3]);
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::PlatinumCoin, 1)));
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::GoldCoin, 2)));
        assert_eq!(inv[2], Some(InvSlot::new(ItemId::SilverCoin, 3)));
        assert_eq!(inv[3], Some(InvSlot::new(ItemId::CopperCoin, 4)));
        assert_eq!(coin_total(&inv), 1_020_304);
    }

    #[test]
    fn add_coins_overflows_when_full() {
        let mut inv = empty();
        for s in inv.iter_mut().take(ARMOR_START) {
            *s = Some(InvSlot::new(ItemId::Stone, 999));
        }
        inv[0] = Some(InvSlot::new(ItemId::CopperCoin, 990));
        let (changed, overflow) = add_coins(&mut inv, 50);
        assert_eq!(changed, vec![0]);
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::CopperCoin, 999)));
        assert_eq!(overflow, vec![InvSlot::new(ItemId::CopperCoin, 41)]);
    }

    #[test]
    fn remove_coins_exact_and_insufficient() {
        let mut inv = empty();
        inv[0] = Some(InvSlot::new(ItemId::CopperCoin, 30));
        inv[1] = Some(InvSlot::new(ItemId::SilverCoin, 2));
        // Insufficient: untouched.
        let before = inv.clone();
        assert_eq!(remove_coins(&mut inv, 231), None);
        assert_eq!(inv, before);
        // Exact spend of the coppers.
        let (changed, overflow) = remove_coins(&mut inv, 30).expect("affordable");
        assert!(overflow.is_empty());
        assert_eq!(changed, vec![0]);
        assert_eq!(inv[0], None);
        assert_eq!(coin_total(&inv), 200);
    }

    #[test]
    fn remove_coins_breaks_larger_denominations_into_change() {
        let mut inv = empty();
        inv[0] = Some(InvSlot::new(ItemId::GoldCoin, 1));
        let (_, overflow) = remove_coins(&mut inv, 1).expect("affordable");
        assert!(overflow.is_empty());
        assert_eq!(coin_total(&inv), 9_999);
        // Change came back as canonical denominations.
        let golds: u32 = count(&inv, ItemId::GoldCoin);
        let silvers: u32 = count(&inv, ItemId::SilverCoin);
        let coppers: u32 = count(&inv, ItemId::CopperCoin);
        assert_eq!((golds, silvers, coppers), (0, 99, 99));
    }

    #[test]
    fn remove_coins_prefers_small_denominations_first() {
        let mut inv = empty();
        inv[0] = Some(InvSlot::new(ItemId::PlatinumCoin, 1));
        inv[1] = Some(InvSlot::new(ItemId::CopperCoin, 100));
        let (_, overflow) = remove_coins(&mut inv, 100).expect("affordable");
        assert!(overflow.is_empty());
        assert_eq!(
            inv[0],
            Some(InvSlot::new(ItemId::PlatinumCoin, 1)),
            "the platinum was never broken"
        );
        assert_eq!(coin_total(&inv), 1_000_000);
    }

    #[test]
    fn remove_coins_change_overflow_is_returned() {
        // Pay 1 CC from a gold coin with every other slot full: the spent
        // gold's slot takes the 99 silver of change, and the 99 copper that
        // has no slot left comes back as overflow for the caller to drop.
        let mut inv = empty();
        for s in inv.iter_mut().take(ARMOR_START) {
            *s = Some(InvSlot::new(ItemId::Stone, 999));
        }
        inv[0] = Some(InvSlot::new(ItemId::GoldCoin, 1));
        let (changed, overflow) = remove_coins(&mut inv, 1).expect("affordable");
        assert_eq!(
            inv[0],
            Some(InvSlot::new(ItemId::SilverCoin, 99)),
            "the gold was spent; its slot took the silver change"
        );
        assert!(changed.contains(&0));
        let spilled: u64 = overflow
            .iter()
            .map(|s| coin_value(s.item) as u64 * s.count as u64)
            .sum();
        assert_eq!(spilled, 99, "the homeless copper change spilled");
        assert_eq!(coin_total(&inv) + spilled, 9_999, "no value lost");
    }

    #[test]
    fn remove_coins_across_multiple_players_worth_of_stacks() {
        // Mixed denominations summing to 50 SC across several stacks.
        let mut inv = empty();
        inv[3] = Some(InvSlot::new(ItemId::SilverCoin, 49));
        inv[7] = Some(InvSlot::new(ItemId::CopperCoin, 100));
        assert_eq!(coin_total(&inv), 5_000);
        let (_, overflow) = remove_coins(&mut inv, 5_000).expect("affordable");
        assert!(overflow.is_empty());
        assert_eq!(coin_total(&inv), 0);
        assert!(inv.iter().flatten().all(|s| !is_coin(s.item)));
    }
}
