//! The slot-op engine: every player-driven slot mutation (`MoveSlot`,
//! `SplitSlot`, `ChestMoveSlot`) funnels through [`apply_slot_op`], which
//! validates region rules (§8 layout, §4.2/§4.3 equipment slots) and stack
//! semantics (§0 stack sizes) over a *slot container view* — the player
//! inventory plus, optionally, the open chest.
//!
//! Shared so the server (authority) and the client (optimistic UI) apply
//! identical rules; the returned deltas are exactly what the server
//! broadcasts as `SlotChanged` / `ChestSlotChanged`.

use crate::items::{inventory, ArmorSlot, InvSlot, ItemId};
use crate::world::CHEST_SLOTS;

/// What a slot position accepts (derived from the §8 layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotRole {
    /// Hotbar/backpack/chest: anything.
    Plain,
    /// One of the three armor slots: only the matching piece.
    Armor(ArmorSlot),
    /// Accessory slot: accessories only, no duplicate effect equipped.
    Accessory,
    /// Trash: accepts anything by destroying what it held.
    Trash,
}

/// Role of a flat player-inventory index; `None` when out of range.
pub fn inventory_role(idx: usize) -> Option<SlotRole> {
    use inventory::*;
    match idx {
        _ if idx < ARMOR_START => Some(SlotRole::Plain),
        _ if idx < ACCESSORY_START => Some(SlotRole::Armor(ARMOR_SLOT_ORDER[idx - ARMOR_START])),
        _ if idx < TRASH => Some(SlotRole::Accessory),
        TRASH => Some(SlotRole::Trash),
        _ => None,
    }
}

/// Addresses one slot of the op's working set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotAddr {
    /// Flat player-inventory index (`items::inventory` layout).
    Inv(usize),
    /// Slot of the open chest (`0..CHEST_SLOTS`).
    Chest(usize),
}

/// The two op shapes the wire knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOp {
    /// Move the whole stack: merge onto a same-item stack (up to its max),
    /// swap with a different item, destroy-and-replace into trash.
    Move,
    /// Move half the stack (rounded up) onto an empty or same-item target —
    /// the RMB half-pickup.
    SplitHalf,
}

/// Why an op was rejected. The op is a no-op on any error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOpError {
    OutOfRange,
    /// A chest address was used with no chest view supplied.
    NoChest,
    SameSlot,
    EmptySource,
    /// Region rules forbid the item in the target (or, on swap, the source).
    Incompatible,
    /// An accessory with the same effect is already equipped (§4.3).
    DuplicateAccessory,
    /// Target stack is already full.
    NoRoom,
}

/// A slot whose contents changed, with its new contents — broadcast-ready.
pub type SlotDelta = (SlotAddr, Option<InvSlot>);

/// Does `role` accept `item` at all? (Duplicate-accessory checks are
/// separate — they need the whole inventory.)
fn accepts(role: SlotRole, item: ItemId) -> bool {
    match role {
        SlotRole::Plain | SlotRole::Trash => true,
        SlotRole::Armor(slot) => item.data().armor.is_some_and(|a| a.slot == slot),
        SlotRole::Accessory => item.data().accessory.is_some(),
    }
}

/// The working set of one op.
struct View<'a> {
    inv: &'a mut [Option<InvSlot>],
    chest: Option<&'a mut [Option<InvSlot>]>,
}

impl View<'_> {
    fn role(&self, addr: SlotAddr) -> Result<SlotRole, SlotOpError> {
        match addr {
            SlotAddr::Inv(i) if i < self.inv.len() => {
                inventory_role(i).ok_or(SlotOpError::OutOfRange)
            }
            SlotAddr::Inv(_) => Err(SlotOpError::OutOfRange),
            SlotAddr::Chest(i) => match &self.chest {
                Some(c) if i < c.len() && i < CHEST_SLOTS => Ok(SlotRole::Plain),
                Some(_) => Err(SlotOpError::OutOfRange),
                None => Err(SlotOpError::NoChest),
            },
        }
    }

    fn get(&self, addr: SlotAddr) -> Option<InvSlot> {
        match addr {
            SlotAddr::Inv(i) => self.inv.get(i).copied().flatten(),
            SlotAddr::Chest(i) => self
                .chest
                .as_ref()
                .and_then(|c| c.get(i))
                .copied()
                .flatten(),
        }
    }

    fn set(&mut self, addr: SlotAddr, v: Option<InvSlot>) {
        match addr {
            SlotAddr::Inv(i) => self.inv[i] = v,
            SlotAddr::Chest(i) => {
                if let Some(c) = self.chest.as_mut() {
                    c[i] = v;
                }
            }
        }
    }

    /// `item` (with accessory effect) entering accessory slot `into` would
    /// duplicate an already-equipped effect. The slot the item comes from is
    /// excluded — it still holds the item while we validate.
    fn dupe_accessory(&self, item: ItemId, into: usize, moving_from: SlotAddr) -> bool {
        let Some(effect) = item.data().accessory else {
            return false;
        };
        (inventory::ACCESSORY_START..inventory::ACCESSORY_START + inventory::ACCESSORY).any(|i| {
            i != into
                && SlotAddr::Inv(i) != moving_from
                && self
                    .get(SlotAddr::Inv(i))
                    .is_some_and(|s| s.item.data().accessory == Some(effect))
        })
    }
}

/// Applies one slot op to the player inventory (`inv`, flat §8 layout) and
/// the optionally-open chest. On success returns the deltas for every slot
/// that changed; on error nothing was touched.
pub fn apply_slot_op(
    inv: &mut [Option<InvSlot>],
    chest: Option<&mut [Option<InvSlot>]>,
    op: SlotOp,
    from: SlotAddr,
    to: SlotAddr,
) -> Result<Vec<SlotDelta>, SlotOpError> {
    let mut view = View { inv, chest };
    if from == to {
        return Err(SlotOpError::SameSlot);
    }
    let from_role = view.role(from)?;
    let to_role = view.role(to)?;
    let src = view.get(from).ok_or(SlotOpError::EmptySource)?;
    let dst = view.get(to);

    let (new_from, new_to) = match op {
        SlotOp::Move => plan_move(&view, src, dst, from, from_role, to, to_role)?,
        SlotOp::SplitHalf => plan_split(&view, src, dst, from, to, to_role)?,
    };

    let mut deltas = Vec::with_capacity(2);
    if new_from != Some(src) {
        view.set(from, new_from);
        deltas.push((from, new_from));
    }
    if new_to != dst {
        view.set(to, new_to);
        deltas.push((to, new_to));
    }
    Ok(deltas)
}

/// Computes the (from, to) contents after a [`SlotOp::Move`].
fn plan_move(
    view: &View<'_>,
    src: InvSlot,
    dst: Option<InvSlot>,
    from: SlotAddr,
    from_role: SlotRole,
    to: SlotAddr,
    to_role: SlotRole,
) -> Result<(Option<InvSlot>, Option<InvSlot>), SlotOpError> {
    // Trash target destroys what it held and takes the stack (§8 layout
    // notes: trash overwrite = destroy), no further checks.
    if to_role == SlotRole::Trash {
        return Ok((None, Some(src)));
    }
    if !accepts(to_role, src.item) {
        return Err(SlotOpError::Incompatible);
    }
    if to_role == SlotRole::Accessory {
        if let SlotAddr::Inv(i) = to {
            if view.dupe_accessory(src.item, i, from) {
                return Err(SlotOpError::DuplicateAccessory);
            }
        }
    }
    match dst {
        None => Ok((None, Some(src))),
        Some(d) if d.item == src.item => {
            // Merge onto the same item, up to its max stack.
            let space = src.item.max_stack().saturating_sub(d.count);
            if space == 0 {
                return Err(SlotOpError::NoRoom);
            }
            let moved = space.min(src.count);
            let left = src.count - moved;
            Ok((
                (left > 0).then_some(InvSlot::new(src.item, left)),
                Some(InvSlot::new(d.item, d.count + moved)),
            ))
        }
        Some(d) => {
            // Swap: the displaced stack must fit the source slot's rules.
            if !accepts(from_role, d.item) {
                return Err(SlotOpError::Incompatible);
            }
            if from_role == SlotRole::Accessory {
                if let SlotAddr::Inv(i) = from {
                    if view.dupe_accessory(d.item, i, to) {
                        return Err(SlotOpError::DuplicateAccessory);
                    }
                }
            }
            Ok((Some(d), Some(src)))
        }
    }
}

/// Computes the (from, to) contents after a [`SlotOp::SplitHalf`].
fn plan_split(
    view: &View<'_>,
    src: InvSlot,
    dst: Option<InvSlot>,
    from: SlotAddr,
    to: SlotAddr,
    to_role: SlotRole,
) -> Result<(Option<InvSlot>, Option<InvSlot>), SlotOpError> {
    let half = src.count.div_ceil(2);
    let keep = |n: u16| (n > 0).then_some(InvSlot::new(src.item, n));
    // Trash keeps its destroy-and-replace semantics even for halves.
    if to_role == SlotRole::Trash {
        return Ok((keep(src.count - half), Some(InvSlot::new(src.item, half))));
    }
    if !accepts(to_role, src.item) {
        return Err(SlotOpError::Incompatible);
    }
    if to_role == SlotRole::Accessory {
        if let SlotAddr::Inv(i) = to {
            if view.dupe_accessory(src.item, i, from) {
                return Err(SlotOpError::DuplicateAccessory);
            }
        }
    }
    match dst {
        None => Ok((keep(src.count - half), Some(InvSlot::new(src.item, half)))),
        Some(d) if d.item == src.item => {
            let space = src.item.max_stack().saturating_sub(d.count);
            if space == 0 {
                return Err(SlotOpError::NoRoom);
            }
            let moved = space.min(half);
            Ok((
                keep(src.count - moved),
                Some(InvSlot::new(d.item, d.count + moved)),
            ))
        }
        Some(_) => Err(SlotOpError::Incompatible),
    }
}

/// First index in `range` a stack of `item` would quick-move onto: a
/// same-item stack with room first, else the first empty slot. Used by the
/// client's shift-click quick-move (hotbar ↔ backpack, inventory ↔ chest).
pub fn quick_move_dest(
    slots: &[Option<InvSlot>],
    item: ItemId,
    range: std::ops::Range<usize>,
) -> Option<usize> {
    let range = range.start..range.end.min(slots.len());
    let max = item.max_stack();
    range
        .clone()
        .find(|&i| slots[i].is_some_and(|s| s.item == item && s.count < max))
        .or_else(|| {
            let mut range = range;
            range.find(|&i| slots[i].is_none())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::inventory::{ACCESSORY_START, ARMOR_START, TOTAL, TRASH};
    use SlotAddr::{Chest, Inv};

    fn inv_with(items: &[(usize, ItemId, u16)]) -> Vec<Option<InvSlot>> {
        let mut slots = vec![None; TOTAL];
        for &(i, item, n) in items {
            slots[i] = Some(InvSlot::new(item, n));
        }
        slots
    }

    fn mv(
        inv: &mut [Option<InvSlot>],
        from: usize,
        to: usize,
    ) -> Result<Vec<SlotDelta>, SlotOpError> {
        apply_slot_op(inv, None, SlotOp::Move, Inv(from), Inv(to))
    }

    #[test]
    fn move_into_empty_and_swap() {
        let mut inv = inv_with(&[(0, ItemId::WoodSword, 1), (1, ItemId::WoodPickaxe, 1)]);
        let deltas = mv(&mut inv, 0, 5).expect("move to empty");
        assert_eq!(inv[0], None);
        assert_eq!(inv[5], Some(InvSlot::new(ItemId::WoodSword, 1)));
        assert_eq!(deltas.len(), 2);

        mv(&mut inv, 1, 5).expect("swap");
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::WoodSword, 1)));
        assert_eq!(inv[5], Some(InvSlot::new(ItemId::WoodPickaxe, 1)));
    }

    #[test]
    fn merge_respects_max_stack() {
        // 600 + 500 dirt: merge fills to 999, leaves 101.
        let mut inv = inv_with(&[(0, ItemId::Dirt, 500), (1, ItemId::Dirt, 600)]);
        let deltas = mv(&mut inv, 0, 1).expect("merge");
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::Dirt, 101)));
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::Dirt, 999)));
        assert_eq!(deltas.len(), 2);

        // Full target: NoRoom, nothing changes.
        let before = inv.clone();
        assert_eq!(mv(&mut inv, 0, 1), Err(SlotOpError::NoRoom));
        assert_eq!(inv, before);

        // Partial merge that empties the source.
        let mut inv = inv_with(&[(0, ItemId::Gel, 10), (1, ItemId::Gel, 20)]);
        mv(&mut inv, 0, 1).expect("merge all");
        assert_eq!(inv[0], None);
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::Gel, 30)));

        // Potions stack to 30, not 999.
        let mut inv = inv_with(&[
            (0, ItemId::LesserHealingPotion, 25),
            (1, ItemId::LesserHealingPotion, 10),
        ]);
        mv(&mut inv, 0, 1).expect("merge potions");
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::LesserHealingPotion, 5)));
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::LesserHealingPotion, 30)));
    }

    #[test]
    fn split_half_rounds_up() {
        let mut inv = inv_with(&[(0, ItemId::Torch, 5)]);
        apply_slot_op(&mut inv, None, SlotOp::SplitHalf, Inv(0), Inv(1)).expect("split");
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::Torch, 2)));
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::Torch, 3)));

        // Splitting 1 moves the whole thing.
        let mut inv = inv_with(&[(0, ItemId::Torch, 1)]);
        apply_slot_op(&mut inv, None, SlotOp::SplitHalf, Inv(0), Inv(1)).expect("split of 1");
        assert_eq!(inv[0], None);
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::Torch, 1)));

        // Split onto a same-item stack merges the half.
        let mut inv = inv_with(&[(0, ItemId::Torch, 8), (1, ItemId::Torch, 4)]);
        apply_slot_op(&mut inv, None, SlotOp::SplitHalf, Inv(0), Inv(1)).expect("split-merge");
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::Torch, 4)));
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::Torch, 8)));

        // Split onto a different item is rejected.
        let mut inv = inv_with(&[(0, ItemId::Torch, 8), (1, ItemId::Gel, 4)]);
        assert_eq!(
            apply_slot_op(&mut inv, None, SlotOp::SplitHalf, Inv(0), Inv(1)),
            Err(SlotOpError::Incompatible)
        );
    }

    #[test]
    fn armor_slots_validate_piece_kind() {
        // Helmet goes into the head slot (ARMOR_START), not chest/legs.
        let mut inv = inv_with(&[(0, ItemId::IronHelmet, 1)]);
        mv(&mut inv, 0, ARMOR_START).expect("helmet equips");
        assert_eq!(inv[ARMOR_START], Some(InvSlot::new(ItemId::IronHelmet, 1)));

        let mut inv = inv_with(&[(0, ItemId::IronHelmet, 1)]);
        assert_eq!(
            mv(&mut inv, 0, ARMOR_START + 1),
            Err(SlotOpError::Incompatible),
            "helmet in chest slot"
        );
        let mut inv = inv_with(&[(0, ItemId::IronGreaves, 1)]);
        mv(&mut inv, 0, ARMOR_START + 2).expect("greaves equip");

        // Non-armor can't be equipped at all.
        let mut inv = inv_with(&[(0, ItemId::Dirt, 10)]);
        assert_eq!(mv(&mut inv, 0, ARMOR_START), Err(SlotOpError::Incompatible));

        // Swapping helmets through the head slot works both ways.
        let mut inv = inv_with(&[
            (0, ItemId::IronHelmet, 1),
            (ARMOR_START, ItemId::GoldHelmet, 1),
        ]);
        mv(&mut inv, 0, ARMOR_START).expect("swap helmets");
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::GoldHelmet, 1)));
        assert_eq!(inv[ARMOR_START], Some(InvSlot::new(ItemId::IronHelmet, 1)));

        // Swap that would put a non-armor item into the armor slot fails.
        let mut inv = inv_with(&[(ARMOR_START, ItemId::GoldHelmet, 1), (0, ItemId::Dirt, 5)]);
        assert_eq!(mv(&mut inv, 0, ARMOR_START), Err(SlotOpError::Incompatible));
    }

    #[test]
    fn accessory_slots_reject_duplicates() {
        let mut inv = inv_with(&[(0, ItemId::SwiftBoots, 1), (1, ItemId::SwiftBoots, 1)]);
        mv(&mut inv, 0, ACCESSORY_START).expect("first boots equip");
        assert_eq!(
            mv(&mut inv, 1, ACCESSORY_START + 1),
            Err(SlotOpError::DuplicateAccessory),
            "second pair of the same accessory"
        );
        // A different accessory is fine.
        let mut inv2 = inv_with(&[
            (ACCESSORY_START, ItemId::SwiftBoots, 1),
            (0, ItemId::GustJar, 1),
        ]);
        mv(&mut inv2, 0, ACCESSORY_START + 1).expect("different accessory");

        // Moving the equipped accessory between accessory slots is not a
        // duplicate of itself.
        let mut inv3 = inv_with(&[(ACCESSORY_START, ItemId::SwiftBoots, 1)]);
        mv(&mut inv3, ACCESSORY_START, ACCESSORY_START + 2).expect("relocate equipped");

        // Non-accessories can't be equipped.
        let mut inv4 = inv_with(&[(0, ItemId::IronHelmet, 1)]);
        assert_eq!(
            mv(&mut inv4, 0, ACCESSORY_START),
            Err(SlotOpError::Incompatible)
        );
    }

    #[test]
    fn trash_overwrites_and_destroys() {
        let mut inv = inv_with(&[(0, ItemId::Dirt, 99), (TRASH, ItemId::GoldBar, 5)]);
        mv(&mut inv, 0, TRASH).expect("trash it");
        assert_eq!(inv[0], None);
        assert_eq!(
            inv[TRASH],
            Some(InvSlot::new(ItemId::Dirt, 99)),
            "old trash destroyed"
        );

        // Items can be taken back out of the trash before it's overwritten.
        mv(&mut inv, TRASH, 3).expect("retrieve");
        assert_eq!(inv[3], Some(InvSlot::new(ItemId::Dirt, 99)));
        assert_eq!(inv[TRASH], None);

        // Trash overwrites even same-item stacks (no merge).
        let mut inv = inv_with(&[(0, ItemId::Dirt, 10), (TRASH, ItemId::Dirt, 999)]);
        mv(&mut inv, 0, TRASH).expect("overwrite same item");
        assert_eq!(inv[TRASH], Some(InvSlot::new(ItemId::Dirt, 10)));
    }

    #[test]
    fn rejects_garbage_addresses_and_empty_sources() {
        let mut inv = inv_with(&[(0, ItemId::Dirt, 1)]);
        assert_eq!(mv(&mut inv, 0, 0), Err(SlotOpError::SameSlot));
        assert_eq!(mv(&mut inv, 1, 2), Err(SlotOpError::EmptySource));
        assert_eq!(mv(&mut inv, 0, TOTAL), Err(SlotOpError::OutOfRange));
        assert_eq!(mv(&mut inv, TOTAL + 5, 0), Err(SlotOpError::OutOfRange));
        assert_eq!(
            apply_slot_op(&mut inv, None, SlotOp::Move, Inv(0), Chest(0)),
            Err(SlotOpError::NoChest)
        );
        let mut chest = vec![None; crate::world::CHEST_SLOTS];
        assert_eq!(
            apply_slot_op(
                &mut inv,
                Some(&mut chest),
                SlotOp::Move,
                Inv(0),
                Chest(crate::world::CHEST_SLOTS)
            ),
            Err(SlotOpError::OutOfRange)
        );
    }

    #[test]
    fn chest_view_moves_both_directions() {
        let mut inv = inv_with(&[(0, ItemId::Wood, 30)]);
        let mut chest = vec![None; crate::world::CHEST_SLOTS];
        chest[7] = Some(InvSlot::new(ItemId::Wood, 990));

        // Inventory -> chest merges into the chest stack (caps at 999).
        let deltas = apply_slot_op(&mut inv, Some(&mut chest), SlotOp::Move, Inv(0), Chest(7))
            .expect("inv -> chest");
        assert_eq!(chest[7], Some(InvSlot::new(ItemId::Wood, 999)));
        assert_eq!(inv[0], Some(InvSlot::new(ItemId::Wood, 21)));
        assert!(deltas.contains(&(Chest(7), Some(InvSlot::new(ItemId::Wood, 999)))));
        assert!(deltas.contains(&(Inv(0), Some(InvSlot::new(ItemId::Wood, 21)))));

        // Chest -> inventory into an empty slot.
        apply_slot_op(&mut inv, Some(&mut chest), SlotOp::Move, Chest(7), Inv(1))
            .expect("chest -> inv");
        assert_eq!(chest[7], None);
        assert_eq!(inv[1], Some(InvSlot::new(ItemId::Wood, 999)));

        // Chest slots are Plain: armor pieces store fine, but a chest slot
        // can never impersonate an equipment slot.
        let mut inv = inv_with(&[(0, ItemId::GoldHelmet, 1)]);
        apply_slot_op(&mut inv, Some(&mut chest), SlotOp::Move, Inv(0), Chest(0))
            .expect("armor stores in chest");
        assert_eq!(chest[0], Some(InvSlot::new(ItemId::GoldHelmet, 1)));
    }

    #[test]
    fn quick_move_prefers_merge_then_empty() {
        let mut slots = vec![None; 10];
        slots[3] = Some(InvSlot::new(ItemId::Torch, 5));
        slots[4] = Some(InvSlot::new(ItemId::Gel, 5));
        assert_eq!(quick_move_dest(&slots, ItemId::Torch, 0..10), Some(3));
        assert_eq!(
            quick_move_dest(&slots, ItemId::Gel, 0..4),
            Some(0),
            "empty fallback"
        );
        slots[3] = Some(InvSlot::new(ItemId::Torch, 999)); // full stack
        assert_eq!(quick_move_dest(&slots, ItemId::Torch, 3..5), None);
        let full: Vec<_> = (0..5)
            .map(|_| Some(InvSlot::new(ItemId::Dirt, 999)))
            .collect();
        assert_eq!(quick_move_dest(&full, ItemId::Gel, 0..5), None);
    }
}
