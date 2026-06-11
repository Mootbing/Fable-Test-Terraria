//! Town NPC data tables (DESIGN §7): stats, arrival conditions, the §7.3
//! merchant shop, §7.4 nurse pricing, and the §7.5 dialogue tables with
//! their condition tags.
//!
//! Shared because both sides need it: the server evaluates arrivals,
//! transactions, and dialogue selection authoritatively; the client renders
//! shop prices, sell-price tooltips, and the live Nurse cost preview from
//! the same numbers.

use crate::items::ItemId;
pub use crate::protocol::NpcKind;
use crate::COPPER_PER_SILVER;

// ---- Stats (§7.1) -----------------------------------------------------------

/// Town NPCs are passive, 250 HP, 15 defense, and fight back for 10 damage
/// when hurt (§7.1).
pub const NPC_HP: u32 = 250;
pub const NPC_DEFENSE: u32 = 15;
pub const NPC_FIGHT_BACK_DAMAGE: u32 = 10;

/// Day wander leash: within 25 tiles of home (§7.1).
pub const NPC_WANDER_RADIUS: f32 = 25.0;

/// Talk/shop/heal interaction range, player center to NPC center: §8 reach
/// (6 tiles) plus a body-width of slack so standing flush always works.
/// Shared because the client's "[E] talk" prompt and panel auto-close must
/// agree with the server's validation.
pub const NPC_TALK_RANGE: f32 = crate::REACH + 1.0;

/// Animation bits carried in NPC `EntityState::state` snapshots.
pub mod anim {
    pub const FACING_RIGHT: u8 = 1 << 0;
    pub const WALKING: u8 = 1 << 1;
}

// ---- Arrival (§7.2) ----------------------------------------------------------

/// Merchant: all players' combined inventory coins ≥ 50 SC (§7.2).
pub const MERCHANT_ARRIVAL_COPPER: u64 = 50 * COPPER_PER_SILVER as u64;
/// Nurse: any player's max HP over 100 — i.e. has used a Life Crystal (§7.2).
pub const NURSE_ARRIVAL_MAX_HP: u32 = 100;

/// Why/when an NPC kind moves in (§7.2). The server evaluates these; the
/// variants carry the data-table thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrivalCondition {
    /// Sage: at world spawn from the start.
    WorldStart,
    /// Merchant: all players' (online and offline) combined coins ≥ this
    /// many copper, plus a vacant valid house.
    CombinedCoinsAtLeast(u64),
    /// Nurse: any player's (online or offline) max HP strictly over this,
    /// the Merchant present, plus a vacant valid house.
    MaxHpOverAndMerchantPresent(u32),
}

// ---- Per-kind data -------------------------------------------------------------

/// Static per-NPC-kind data; one row per [`NpcKind`] in [`NPC_DATA`].
#[derive(Debug)]
pub struct NpcData {
    /// Nameplate, "<name> the <role>" style. DESIGN names the roles
    /// (Guide/Merchant/Nurse); the given names are canonized here.
    pub display_name: &'static str,
    /// Kind name for announcements ("The Merchant has arrived!").
    pub kind_name: &'static str,
    pub arrival: ArrivalCondition,
    /// §7.5 dialogue table, verbatim.
    pub lines: &'static [DialogueLine],
}

pub const NPC_KINDS: [NpcKind; 3] = [NpcKind::Sage, NpcKind::Merchant, NpcKind::Nurse];

pub fn npc_data(kind: NpcKind) -> &'static NpcData {
    match kind {
        NpcKind::Sage => &SAGE,
        NpcKind::Merchant => &MERCHANT,
        NpcKind::Nurse => &NURSE,
    }
}

// ---- Dialogue (§7.5) -------------------------------------------------------------

/// Condition tag on a dialogue line (§7.5 parentheticals). `Default` lines
/// are always eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogueCondition {
    Default,
    Night,
    /// Night, before The Watcher has been defeated.
    NightPreWatcher,
    /// Talking player's HP < 30%.
    LowHp,
    /// Talking player at full HP.
    FullHp,
    /// Any boss currently alive.
    BossAlive,
    /// The Watcher defeated (world flag).
    AfterWatcher,
    /// Any boss defeated (world flag).
    AfterAnyBoss,
    /// This NPC has no assigned house.
    Homeless,
    /// Talking player carries > 1 GC in coins.
    RichPlayer,
    /// Talking player has Potion Sickness active.
    PotionSick,
    /// Slime Monarch defeated (world flag).
    SlimeMonarchDefeated,
}

/// One §7.5 line with its condition tag.
#[derive(Debug, Clone, Copy)]
pub struct DialogueLine {
    pub cond: DialogueCondition,
    pub text: &'static str,
}

const fn line(cond: DialogueCondition, text: &'static str) -> DialogueLine {
    DialogueLine { cond, text }
}

/// The live state a dialogue pick is evaluated against — built by the server
/// from world time/flags and the talking player, and by the client preview
/// from its synced mirrors.
#[derive(Debug, Clone, Copy, Default)]
pub struct DialogueCtx {
    pub night: bool,
    pub boss_alive: bool,
    pub watcher_defeated: bool,
    pub slime_monarch_defeated: bool,
    pub bone_warden_defeated: bool,
    /// Talking player's HP < 30% of max.
    pub low_hp: bool,
    /// Talking player at full HP.
    pub full_hp: bool,
    /// Talking player carries > 1 GC in coins.
    pub rich: bool,
    /// Talking player has Potion Sickness.
    pub potion_sick: bool,
    /// The NPC being talked to has no house.
    pub homeless: bool,
}

impl DialogueCtx {
    pub fn any_boss_defeated(&self) -> bool {
        self.watcher_defeated || self.slime_monarch_defeated || self.bone_warden_defeated
    }
}

/// Does `cond` hold in `ctx`? (`Default` always does.)
pub fn condition_holds(cond: DialogueCondition, ctx: &DialogueCtx) -> bool {
    use DialogueCondition::*;
    match cond {
        Default => true,
        Night => ctx.night,
        NightPreWatcher => ctx.night && !ctx.watcher_defeated,
        LowHp => ctx.low_hp,
        FullHp => ctx.full_hp,
        BossAlive => ctx.boss_alive,
        AfterWatcher => ctx.watcher_defeated,
        AfterAnyBoss => ctx.any_boss_defeated(),
        Homeless => ctx.homeless,
        RichPlayer => ctx.rich,
        PotionSick => ctx.potion_sick,
        SlimeMonarchDefeated => ctx.slime_monarch_defeated,
    }
}

/// All lines of `kind` whose condition holds (`Default` always eligible —
/// the pool is never empty).
pub fn eligible_lines(kind: NpcKind, ctx: &DialogueCtx) -> Vec<&'static str> {
    npc_data(kind)
        .lines
        .iter()
        .filter(|l| condition_holds(l.cond, ctx))
        .map(|l| l.text)
        .collect()
}

/// Uniform pick among the eligible lines: `roll` is any random u32; the
/// caller supplies the entropy so selection stays pure and testable.
pub fn pick_line(kind: NpcKind, ctx: &DialogueCtx, roll: u32) -> &'static str {
    let pool = eligible_lines(kind, ctx);
    pool[roll as usize % pool.len()]
}

use DialogueCondition as C;

static SAGE: NpcData = NpcData {
    display_name: "Sage the Guide",
    kind_name: "Sage",
    arrival: ArrivalCondition::WorldStart,
    lines: &[
        line(
            C::Default,
            "You can press buttons to chop trees. Wood builds everything — start with a workbench.",
        ),
        line(
            C::Default,
            "If you see a pot, smash it. If you see a heart-shaped crystal, REALLY smash it.",
        ),
        line(
            C::Default,
            "Furnaces smelt ore into bars. Three copper ore per bar — the deeper metals cost four.",
        ),
        line(
            C::Night,
            "Keep your walls sealed at night. Zombies can't open doors, but they're patient.",
        ),
        line(
            C::NightPreWatcher,
            "Sometimes I feel an enormous gaze on the back of my neck. Probably nothing.",
        ),
        line(
            C::LowHp,
            "You look terrible. Gel, a mushroom, and a bottle make a healing potion — write that down.",
        ),
        line(
            C::BossAlive,
            "Less talking, more fighting! I'll be under this table.",
        ),
        line(
            C::AfterWatcher,
            "You actually beat it. The old stories say a crowned slime and a buried warden remain.",
        ),
        line(
            C::Homeless,
            "Build me a room — walls, a door, a light, a table, a chair. I'm not picky. That's a lie, I'm exactly that picky.",
        ),
    ],
};

static MERCHANT: NpcData = NpcData {
    display_name: "Fenwick the Merchant",
    kind_name: "Merchant",
    arrival: ArrivalCondition::CombinedCoinsAtLeast(MERCHANT_ARRIVAL_COPPER),
    lines: &[
        line(
            C::Default,
            "Everything's for sale, friend. Even my respect — that one's pricey.",
        ),
        line(
            C::Default,
            "Torches! Fifty copper! Darkness is free and look where THAT gets you.",
        ),
        line(
            C::Default,
            "I buy junk at a fifth of its worth. It's not a scam, it's logistics.",
        ),
        line(
            C::Night,
            "We're open all night. Mostly because I can't sleep with all that moaning outside.",
        ),
        line(
            C::RichPlayer,
            "Is that gold I smell? My prices may have just... matured.",
        ),
        line(
            C::BossAlive,
            "Shop's open but the refund window is CLOSED until that thing stops screaming.",
        ),
        line(
            C::LowHp,
            "No bleeding on the merchandise. Healing potion, three silver. A bargain, given the alternative.",
        ),
        line(
            C::SlimeMonarchDefeated,
            "You sold me forty units of royal gel. I respect the hustle.",
        ),
    ],
};

static NURSE: NpcData = NpcData {
    display_name: "Mira the Nurse",
    kind_name: "Nurse",
    arrival: ArrivalCondition::MaxHpOverAndMerchantPresent(NURSE_ARRIVAL_MAX_HP),
    lines: &[
        line(
            C::Default,
            "Walk it off? No. Pay me, and I'll fix it properly.",
        ),
        line(
            C::Default,
            "I've seen every injury this world can produce. You're going to show me a new one, aren't you.",
        ),
        line(
            C::LowHp,
            "Sit. Down. Now. You're getting blood on the floor I just had.",
        ),
        line(
            C::FullHp,
            "You're fine. Stop wasting my time and go get hurt somewhere.",
        ),
        line(
            C::Night,
            "Night shift again. The screaming outside really completes the clinic ambiance.",
        ),
        line(
            C::BossAlive,
            "Triage rules: heroes first, cowards pay double. Kidding. Mostly.",
        ),
        line(
            C::AfterAnyBoss,
            "Fewer monsters means fewer patients. Don't take that personally.",
        ),
        line(
            C::PotionSick,
            "I can't purge potion sickness — your liver and I have an agreement.",
        ),
    ],
};

/// "Player coins > 1 GC" threshold for [`DialogueCondition::RichPlayer`].
pub const RICH_PLAYER_COPPER: u64 = crate::COPPER_PER_GOLD as u64;
/// "Player HP < 30%" threshold for [`DialogueCondition::LowHp`], in percent.
pub const LOW_HP_PERCENT: u32 = 30;

// ---- Merchant shop (§7.3) -----------------------------------------------------

/// One §7.3 stock row. Prices are in copper and equal the item's base
/// `ItemData::value` (the §7.3 table pins those values; a test asserts it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShopItem {
    pub item: ItemId,
    pub price: u32,
}

pub const MERCHANT_SHOP: [ShopItem; 8] = [
    ShopItem {
        item: ItemId::Torch,
        price: 50,
    },
    ShopItem {
        item: ItemId::Bottle,
        price: 20,
    },
    ShopItem {
        item: ItemId::WoodenArrow,
        price: 5,
    },
    ShopItem {
        item: ItemId::LesserHealingPotion,
        price: 3 * COPPER_PER_SILVER,
    },
    ShopItem {
        item: ItemId::CopperPickaxe,
        price: 5 * COPPER_PER_SILVER,
    },
    ShopItem {
        item: ItemId::CopperAxe,
        price: 4 * COPPER_PER_SILVER,
    },
    ShopItem {
        item: ItemId::Anvil,
        price: 50 * COPPER_PER_SILVER,
    },
    ShopItem {
        item: ItemId::MiningHelmet,
        price: 4 * crate::COPPER_PER_GOLD,
    },
];

/// Buy price of `item` at the merchant, if stocked.
pub fn shop_price(item: ItemId) -> Option<u32> {
    MERCHANT_SHOP
        .iter()
        .find(|s| s.item == item)
        .map(|s| s.price)
}

/// The merchant buys anything back at 20% of base value (§7.3), rounded
/// down. 0 means unsellable: items with no value, and coins — "buys back
/// any item" doesn't extend to currency, whose value *is* its face value
/// (selling money at 20% would just destroy 80% of it).
pub const SELL_PRICE_PERCENT: u32 = 20;

pub fn sell_price(item: ItemId) -> u32 {
    if crate::coins::is_coin(item) {
        return 0;
    }
    item.data().value * SELL_PRICE_PERCENT / 100
}

// ---- Nurse pricing (§7.4) -------------------------------------------------------

/// Minimum charge for any (non-no-op) heal, in copper.
pub const NURSE_MIN_COST: u64 = 10;
/// 1 CC per HP restored.
pub const NURSE_COST_PER_HP: u64 = 1;
/// 1 SC per debuff cleared.
pub const NURSE_COST_PER_DEBUFF: u64 = COPPER_PER_SILVER as u64;
/// Price multipliers once The Watcher / The Bone Warden are defeated. The
/// multipliers don't stack — the Bone Warden's ×10 supersedes the Watcher's
/// ×3 (§7.4 reads as escalating tiers, not a ×30 product).
pub const NURSE_WATCHER_MULT: u64 = 3;
pub const NURSE_BONE_WARDEN_MULT: u64 = 10;

/// §7.4 heal cost in copper: `1 CC × HP restored + 1 SC per debuff cleared`,
/// ×3 once The Watcher is defeated, ×10 once The Bone Warden is, minimum
/// 10 CC. The caller handles the full-HP no-op (no charge) case itself.
pub fn nurse_heal_cost(
    hp_restored: u32,
    debuffs_cleared: u32,
    watcher_defeated: bool,
    bone_warden_defeated: bool,
) -> u64 {
    let base =
        hp_restored as u64 * NURSE_COST_PER_HP + debuffs_cleared as u64 * NURSE_COST_PER_DEBUFF;
    let mult = if bone_warden_defeated {
        NURSE_BONE_WARDEN_MULT
    } else if watcher_defeated {
        NURSE_WATCHER_MULT
    } else {
        1
    };
    (base * mult).max(NURSE_MIN_COST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shop_prices_match_design_7_3_and_item_values() {
        let expect: [(ItemId, u32); 8] = [
            (ItemId::Torch, 50),
            (ItemId::Bottle, 20),
            (ItemId::WoodenArrow, 5),
            (ItemId::LesserHealingPotion, 300),
            (ItemId::CopperPickaxe, 500),
            (ItemId::CopperAxe, 400),
            (ItemId::Anvil, 5_000),
            (ItemId::MiningHelmet, 40_000),
        ];
        assert_eq!(MERCHANT_SHOP.len(), expect.len());
        for (i, (item, price)) in expect.into_iter().enumerate() {
            assert_eq!(MERCHANT_SHOP[i].item, item);
            assert_eq!(MERCHANT_SHOP[i].price, price, "{item:?}");
            // §7.3 pins these items' base values to their shop price.
            assert_eq!(item.data().value, price, "{item:?} value");
            assert_eq!(shop_price(item), Some(price));
        }
        assert_eq!(shop_price(ItemId::Dirt), None);
    }

    #[test]
    fn sell_prices_are_20_percent_rounded_down() {
        assert_eq!(sell_price(ItemId::Torch), 10); // 50 × 0.2
        assert_eq!(sell_price(ItemId::WoodenArrow), 1); // 5 × 0.2
        assert_eq!(sell_price(ItemId::Gel), 1); // 5 × 0.2
        assert_eq!(sell_price(ItemId::Wood), 2); // 10 × 0.2
        assert_eq!(sell_price(ItemId::Bottle), 4); // 20 × 0.2
        assert_eq!(sell_price(ItemId::Dirt), 0); // value 0: unsellable
        assert_eq!(sell_price(ItemId::WoodenArrow), 1);
        // Rounding down: value 9 would be 1 (9*20/100 = 1.8 → 1); the
        // smallest valued items demonstrate the floor.
        assert_eq!(sell_price(ItemId::Platform), 0); // 4 × 0.2 = 0.8 → 0
    }

    #[test]
    fn coins_are_unsellable() {
        // A coin's `value` is its denomination, so without the exemption
        // the routine shift-click gesture would sell money at 20% of face
        // value and irreversibly destroy the rest.
        for (coin, _) in crate::coins::COIN_DENOMS {
            assert_eq!(sell_price(coin), 0, "{coin:?}");
        }
    }

    #[test]
    fn nurse_cost_formula() {
        // 1 CC per HP.
        assert_eq!(nurse_heal_cost(100, 0, false, false), 100);
        // +1 SC per debuff.
        assert_eq!(nurse_heal_cost(100, 2, false, false), 300);
        // ×3 after Watcher, ×10 after Bone Warden (not ×30).
        assert_eq!(nurse_heal_cost(100, 0, true, false), 300);
        assert_eq!(nurse_heal_cost(100, 0, false, true), 1_000);
        assert_eq!(nurse_heal_cost(100, 0, true, true), 1_000);
        // Minimum 10 CC.
        assert_eq!(nurse_heal_cost(1, 0, false, false), 10);
        assert_eq!(nurse_heal_cost(3, 0, true, false), 10);
        assert_eq!(nurse_heal_cost(0, 1, false, false), 100);
    }

    #[test]
    fn npc_stats_match_design_7_1() {
        assert_eq!(NPC_HP, 250);
        assert_eq!(NPC_DEFENSE, 15);
        assert_eq!(NPC_FIGHT_BACK_DAMAGE, 10);
        assert_eq!(NPC_WANDER_RADIUS, 25.0);
        assert_eq!(MERCHANT_ARRIVAL_COPPER, 5_000);
    }

    #[test]
    fn default_lines_always_eligible_and_only_them_by_default() {
        for kind in NPC_KINDS {
            let ctx = DialogueCtx::default();
            let pool = eligible_lines(kind, &ctx);
            let defaults = npc_data(kind)
                .lines
                .iter()
                .filter(|l| l.cond == DialogueCondition::Default)
                .count();
            assert_eq!(pool.len(), defaults, "{kind:?} default fallback");
            assert!(!pool.is_empty());
            // pick_line covers the whole pool uniformly by roll.
            for roll in 0..pool.len() as u32 {
                assert_eq!(pick_line(kind, &ctx, roll), pool[roll as usize]);
            }
        }
    }

    /// Every conditional §7.5 line is reachable under a ctx that satisfies
    /// exactly its trigger.
    #[test]
    fn each_conditional_line_is_reachable() {
        use DialogueCondition::*;
        let ctx_for = |cond: DialogueCondition| -> DialogueCtx {
            let mut ctx = DialogueCtx::default();
            match cond {
                Default => {}
                Night => ctx.night = true,
                NightPreWatcher => ctx.night = true,
                LowHp => ctx.low_hp = true,
                FullHp => ctx.full_hp = true,
                BossAlive => ctx.boss_alive = true,
                AfterWatcher => ctx.watcher_defeated = true,
                AfterAnyBoss => ctx.bone_warden_defeated = true,
                Homeless => ctx.homeless = true,
                RichPlayer => ctx.rich = true,
                PotionSick => ctx.potion_sick = true,
                SlimeMonarchDefeated => ctx.slime_monarch_defeated = true,
            }
            ctx
        };
        for kind in NPC_KINDS {
            for l in npc_data(kind).lines {
                let ctx = ctx_for(l.cond);
                assert!(
                    eligible_lines(kind, &ctx).contains(&l.text),
                    "{kind:?} line {:?} unreachable",
                    l.text
                );
            }
        }
    }

    #[test]
    fn condition_matrix() {
        use DialogueCondition::*;
        let mut ctx = DialogueCtx {
            night: true,
            ..DialogueCtx::default()
        };
        assert!(condition_holds(Night, &ctx));
        assert!(condition_holds(NightPreWatcher, &ctx));
        ctx.watcher_defeated = true;
        assert!(
            !condition_holds(NightPreWatcher, &ctx),
            "pre-Watcher line retires once it's defeated"
        );
        assert!(condition_holds(AfterWatcher, &ctx));
        assert!(condition_holds(AfterAnyBoss, &ctx));
        let slime = DialogueCtx {
            slime_monarch_defeated: true,
            ..DialogueCtx::default()
        };
        assert!(condition_holds(AfterAnyBoss, &slime));
        assert!(condition_holds(SlimeMonarchDefeated, &slime));
        assert!(!condition_holds(AfterWatcher, &slime));
        assert!(!condition_holds(Night, &DialogueCtx::default()));
        assert!(!condition_holds(BossAlive, &DialogueCtx::default()));
        assert!(condition_holds(Default, &DialogueCtx::default()));
    }

    /// §7.5 table shape: 9 Sage lines, 8 Merchant, 8 Nurse, verbatim spot
    /// checks.
    #[test]
    fn line_tables_match_design_7_5() {
        assert_eq!(npc_data(NpcKind::Sage).lines.len(), 9);
        assert_eq!(npc_data(NpcKind::Merchant).lines.len(), 8);
        assert_eq!(npc_data(NpcKind::Nurse).lines.len(), 8);
        assert_eq!(
            npc_data(NpcKind::Sage).lines[6].text,
            "Less talking, more fighting! I'll be under this table."
        );
        assert_eq!(
            npc_data(NpcKind::Merchant).lines[2].text,
            "I buy junk at a fifth of its worth. It's not a scam, it's logistics."
        );
        assert_eq!(
            npc_data(NpcKind::Nurse).lines[7].text,
            "I can't purge potion sickness — your liver and I have an agreement."
        );
    }
}
