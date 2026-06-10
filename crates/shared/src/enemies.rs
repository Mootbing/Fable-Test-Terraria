//! Enemy definitions: the static [`ENEMY_DATA`] table (DESIGN §5.1), the AI
//! archetype constants (§5.2), the spawning algorithm parameters (§5.3), and
//! the enemy-projectile tuning (§5.2 Void Sickle; arrows live in
//! [`crate::items`]).
//!
//! Pure data + tiny pure helpers only — the server's AI/spawn systems and
//! the client's rendering both read this table, so the numbers live here
//! (ARCHITECTURE.md: "no magic numbers inline in sim code").

use crate::items::ItemId;
use crate::macros::id_table;
use crate::physics::hitbox;
use crate::rng::Pcg32;

// ---- Despawn rectangle (§5.1) ----------------------------------------------

/// Enemies despawn at >168 tiles horizontal or >94 vertical from the nearest
/// player (i.e. once outside every player's despawn rectangle).
pub const DESPAWN_RANGE_X: f32 = 168.0;
pub const DESPAWN_RANGE_Y: f32 = 94.0;

// ---- AI archetype constants (§5.2) ------------------------------------------

/// Slime: idle 0.7–2.0 s between hops.
pub const SLIME_IDLE_MIN_SECS: f32 = 0.7;
pub const SLIME_IDLE_MAX_SECS: f32 = 2.0;
/// Hop: vx 5.6 t/s toward the target, vy 21 t/s (≈2.4 tile apex).
pub const SLIME_HOP_VX: f32 = 5.6;
pub const SLIME_HOP_VY: f32 = 21.0;
/// Every 3rd hop is high: vy 26 t/s (≈3.7 tiles).
pub const SLIME_HIGH_HOP_VY: f32 = 26.0;
pub const SLIME_HIGH_HOP_EVERY: u32 = 3;
/// Lava slimes bounce 1.5× higher out of lava.
pub const LAVA_SLIME_BOUNCE_MULT: f32 = 1.5;
/// Buoyancy acceleration while floating in the slime's float liquid
/// (water; lava for lava slimes) and the upward bob speed cap. DESIGN says
/// only "floats on water"; the magnitudes are canonized here.
pub const SLIME_BUOYANCY_ACCEL: f32 = 180.0;
pub const SLIME_FLOAT_MAX_RISE: f32 = 6.0;

/// Fighter walk speeds: Zombie 3.2 t/s, Skeleton 3.8 t/s.
pub const ZOMBIE_WALK_SPEED: f32 = 3.2;
pub const SKELETON_WALK_SPEED: f32 = 3.8;
/// Blocked horizontally while grounded → jump vy 21 t/s (clears ~2.5 tiles).
pub const FIGHTER_JUMP_VY: f32 = 21.0;

/// Flier-bouncer (Demon Eye): accel 18 t/s², max 9.4 t/s, turn ≤ 90°/s.
pub const BOUNCER_ACCEL: f32 = 18.0;
pub const BOUNCER_MAX_SPEED: f32 = 9.4;
pub const BOUNCER_TURN_RATE_DEG: f32 = 90.0;
/// On tile collision: reflect velocity and add vy −7.5 t/s (bounce up).
pub const BOUNCER_BOUNCE_UP: f32 = 7.5;

/// Flier-erratic (Cave Bat): seek at max 12 t/s; every 0.25–0.6 s add a
/// random jitter of up to ±6 t/s per axis.
pub const ERRATIC_MAX_SPEED: f32 = 12.0;
pub const ERRATIC_JITTER_MIN_SECS: f32 = 0.25;
pub const ERRATIC_JITTER_MAX_SECS: f32 = 0.6;
pub const ERRATIC_JITTER_SPEED: f32 = 6.0;
/// Seek acceleration toward the target (canonized; DESIGN pins only the
/// max speed and jitter).
pub const ERRATIC_ACCEL: f32 = 30.0;

/// Watchling: no jitter, straight at the player at 10.5 t/s.
pub const WATCHLING_SPEED: f32 = 10.5;

/// Swooper (Ash Demon): hovers 8–12 tiles from the player...
pub const SWOOPER_HOVER_MIN: f32 = 8.0;
pub const SWOOPER_HOVER_MAX: f32 = 12.0;
/// ...swoops through at 14 t/s, then retreats...
pub const SWOOPER_SWOOP_SPEED: f32 = 14.0;
/// ...and fires a volley of 4 Void Sickles every 4 s given line of sight.
pub const SWOOPER_VOLLEY_PERIOD_SECS: f32 = 4.0;
pub const SWOOPER_VOLLEY_COUNT: u32 = 4;
/// Hover steering acceleration / max speed (canonized).
pub const SWOOPER_HOVER_ACCEL: f32 = 25.0;
pub const SWOOPER_HOVER_MAX_SPEED: f32 = 8.0;
/// Time spent swooping before switching back to hover (canonized: enough to
/// pass through the player from the hover ring).
pub const SWOOPER_SWOOP_SECS: f32 = 1.4;
/// Cadence between swoops (canonized).
pub const SWOOPER_SWOOP_PERIOD_SECS: f32 = 5.0;

// ---- Void Sickle projectile (§5.2) -------------------------------------------

/// 30 damage, starts at 6 t/s accelerating at 15 t/s² up to 25 t/s,
/// destroyed by tiles, 33% chance of Darkness 5 s.
pub const VOID_SICKLE_DAMAGE: u16 = 30;
pub const VOID_SICKLE_START_SPEED: f32 = 6.0;
pub const VOID_SICKLE_ACCEL: f32 = 15.0;
pub const VOID_SICKLE_MAX_SPEED: f32 = 25.0;
pub const VOID_SICKLE_DARKNESS_CHANCE: f32 = 0.33;
pub const VOID_SICKLE_DARKNESS_SECS: f32 = 5.0;
/// Safety lifetime so a sickle gliding through open air can't live forever
/// (canonized; tiles destroy it long before this in normal play).
pub const VOID_SICKLE_LIFETIME_SECS: f32 = 10.0;

// ---- Enemy knockback response -------------------------------------------------

/// A knockback of `k` tiles/s sets the victim's velocity to `±k`
/// horizontally and `−k ×` this vertically (the §4.1 numbers are horizontal
/// magnitudes; the small pop-up is canonized here).
pub const KNOCKBACK_UP_MULT: f32 = 0.5;

/// Knockback dealt *to players* by enemy contact and enemy projectiles,
/// tiles/s (canonized — DESIGN pins player i-frames but not the shove;
/// applied client-side via `ServerMessage::PlayerKnockback`).
pub const PLAYER_KNOCKBACK_SPEED: f32 = 8.0;

// ---- Burning on enemies --------------------------------------------------------

/// Enemies burn at the same §8 rate as players (2 dmg/s, ignores defense) —
/// the Ember Blade / flaming-arrow proc targets are enemies.
pub const ENEMY_BURNING_DPS: u32 = 2;

// ---- Roster (§5.1) --------------------------------------------------------------

/// AI archetype (§5.2). The per-archetype systems live server-side; this tag
/// picks which one runs (and what placement a spawn needs, §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AiKind {
    /// Grounded hopper (Green/Blue/Lava Slime).
    Slime,
    /// Grounded walker (Zombie, Skeleton).
    Fighter,
    /// Flier — bouncer (Demon Eye).
    FlierBouncer,
    /// Flier — erratic (Cave Bat).
    FlierErratic,
    /// Flier — straight (Watchling).
    FlierStraight,
    /// Swooper + caster (Ash Demon).
    Swooper,
}

impl AiKind {
    /// Grounded archetypes need a floor spawn (solid tile + 3×2 air above);
    /// fliers need a 2×2 air pocket (§5.3 step 4).
    pub fn grounded(self) -> bool {
        matches!(self, AiKind::Slime | AiKind::Fighter)
    }
}

/// One §5.1 drop-table row: `chance` of `min..=max` of `item`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropRow {
    pub item: ItemId,
    pub chance: f32,
    pub min: u16,
    pub max: u16,
}

const fn drop(item: ItemId, chance: f32, min: u16, max: u16) -> DropRow {
    DropRow {
        item,
        chance,
        min,
        max,
    }
}

/// Static per-species stats (§5.1).
#[derive(Debug, Clone, PartialEq)]
pub struct EnemyData {
    pub name: &'static str,
    pub max_hp: u16,
    pub contact_damage: u16,
    pub defense: u16,
    /// Knockback resistance: 0.5 = takes half knockback; −0.2 = takes 20%
    /// *extra* (Green Slime).
    pub kb_resist: f32,
    pub ai: AiKind,
    /// Coin drop in copper, before the ×0.8–1.2 variance.
    pub coins: u32,
    /// Item drops, rolled independently.
    pub drops: &'static [DropRow],
    /// Hitbox (w, h) in tiles.
    pub size: (f32, f32),
}

id_table! {
    /// Every §5.1 enemy species. Watchling is reserved for The Watcher
    /// (§6.2 boss minion; never naturally spawned).
    pub enum EnemyKind(u8), pub table ENEMY_DATA: EnemyData {
        GreenSlime => EnemyData {
            name: "Green Slime",
            max_hp: 14, contact_damage: 6, defense: 0, kb_resist: -0.20,
            ai: AiKind::Slime, coins: 5,
            drops: &[drop(ItemId::Gel, 1.0, 1, 2)],
            size: hitbox::GREEN_SLIME,
        },
        BlueSlime => EnemyData {
            name: "Blue Slime",
            max_hp: 25, contact_damage: 7, defense: 2, kb_resist: 0.0,
            ai: AiKind::Slime, coins: 25,
            drops: &[drop(ItemId::Gel, 1.0, 1, 2)],
            size: hitbox::BLUE_SLIME,
        },
        Zombie => EnemyData {
            name: "Zombie",
            max_hp: 45, contact_damage: 14, defense: 6, kb_resist: 0.50,
            ai: AiKind::Fighter, coins: 60,
            drops: &[drop(ItemId::Wood, 0.50, 1, 1), drop(ItemId::ZombieArm, 0.02, 1, 1)],
            size: hitbox::ZOMBIE,
        },
        DemonEye => EnemyData {
            name: "Demon Eye",
            max_hp: 60, contact_damage: 18, defense: 2, kb_resist: 0.20,
            ai: AiKind::FlierBouncer, coins: 75,
            drops: &[drop(ItemId::Lens, 0.33, 1, 1)],
            size: hitbox::DEMON_EYE,
        },
        CaveBat => EnemyData {
            name: "Cave Bat",
            max_hp: 16, contact_damage: 13, defense: 2, kb_resist: 0.20,
            ai: AiKind::FlierErratic, coins: 90,
            drops: &[],
            size: hitbox::CAVE_BAT,
        },
        Skeleton => EnemyData {
            name: "Skeleton",
            max_hp: 60, contact_damage: 20, defense: 8, kb_resist: 0.50,
            ai: AiKind::Fighter, coins: 100,
            drops: &[drop(ItemId::Bone, 0.50, 1, 3)],
            size: hitbox::SKELETON,
        },
        LavaSlime => EnemyData {
            name: "Lava Slime",
            max_hp: 50, contact_damage: 15, defense: 10, kb_resist: 0.0,
            ai: AiKind::Slime, coins: 120,
            drops: &[], // no gel (§5.1)
            size: hitbox::LAVA_SLIME,
        },
        AshDemon => EnemyData {
            name: "Ash Demon",
            max_hp: 120, contact_damage: 32, defense: 8, kb_resist: 0.20,
            ai: AiKind::Swooper, coins: 300,
            drops: &[drop(ItemId::VoidSickle, 0.0286, 1, 1)],
            size: hitbox::ASH_DEMON,
        },
        /// Boss minion (§6.2) — spawned by The Watcher, never naturally.
        Watchling => EnemyData {
            name: "Watchling",
            max_hp: 8, contact_damage: 12, defense: 0, kb_resist: 0.0,
            ai: AiKind::FlierStraight, coins: 0,
            drops: &[],
            size: hitbox::WATCHLING,
        },
    }
}

impl EnemyKind {
    /// The wire kind for this species ([`crate::protocol::EntityKind`]
    /// declares one variant per enemy, in the same order).
    pub fn wire_kind(self) -> crate::protocol::EntityKind {
        use crate::protocol::EntityKind as E;
        match self {
            EnemyKind::GreenSlime => E::GreenSlime,
            EnemyKind::BlueSlime => E::BlueSlime,
            EnemyKind::Zombie => E::Zombie,
            EnemyKind::DemonEye => E::DemonEye,
            EnemyKind::CaveBat => E::CaveBat,
            EnemyKind::Skeleton => E::Skeleton,
            EnemyKind::LavaSlime => E::LavaSlime,
            EnemyKind::AshDemon => E::AshDemon,
            EnemyKind::Watchling => E::Watchling,
        }
    }

    /// Inverse of [`EnemyKind::wire_kind`] (None for non-enemy wire kinds).
    pub fn from_wire(kind: crate::protocol::EntityKind) -> Option<EnemyKind> {
        use crate::protocol::EntityKind as E;
        Some(match kind {
            E::GreenSlime => EnemyKind::GreenSlime,
            E::BlueSlime => EnemyKind::BlueSlime,
            E::Zombie => EnemyKind::Zombie,
            E::DemonEye => EnemyKind::DemonEye,
            E::CaveBat => EnemyKind::CaveBat,
            E::Skeleton => EnemyKind::Skeleton,
            E::LavaSlime => EnemyKind::LavaSlime,
            E::AshDemon => EnemyKind::AshDemon,
            E::Watchling => EnemyKind::Watchling,
            _ => return None,
        })
    }

    /// Fighter walk speed (§5.2); slimes/fliers don't use it.
    pub fn walk_speed(self) -> f32 {
        match self {
            EnemyKind::Skeleton => SKELETON_WALK_SPEED,
            _ => ZOMBIE_WALK_SPEED,
        }
    }

    /// Slimes are passive on the surface during the day until damaged
    /// (§5.1); only green/blue qualify (lava slimes live underground).
    pub fn day_passive_slime(self) -> bool {
        matches!(self, EnemyKind::GreenSlime | EnemyKind::BlueSlime)
    }

    /// Zombies and Demon Eyes flee and despawn at dawn (§5.2/§9).
    pub fn flees_at_dawn(self) -> bool {
        matches!(self, EnemyKind::Zombie | EnemyKind::DemonEye)
    }
}

// ---- Coin drop variance (§5.1) --------------------------------------------------

/// Coin drops vary ×(0.8–1.2).
pub const COIN_VARIANCE_MIN: f32 = 0.8;
pub const COIN_VARIANCE_MAX: f32 = 1.2;

/// Applies the §5.1 ×0.8–1.2 variance to a coin value.
pub fn coin_drop_value(rng: &mut Pcg32, base: u32) -> u32 {
    if base == 0 {
        return 0;
    }
    (base as f32 * rng.gen_range_f32(COIN_VARIANCE_MIN, COIN_VARIANCE_MAX)).round() as u32
}

// ---- Spawning (§5.3) --------------------------------------------------------------

/// Where a player is for spawn purposes. Row thresholds are the §5.3 table's
/// (and §1.1's layer rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnEnvironment {
    SurfaceDay,
    SurfaceNight,
    Underground,
    Caverns,
    Underworld,
}

/// First row of the §5.3 "Underground" band (rows 341–449).
pub const UNDERGROUND_START_ROW: u32 = 341;
/// First row of the caverns band (450–999).
pub const CAVERNS_START_ROW: u32 = 450;
/// First row of the underworld band (1000+).
pub const UNDERWORLD_START_ROW: u32 = 1000;

/// Classifies a player position + time of day (§5.3 step 1).
pub fn spawn_environment(row: u32, is_day: bool) -> SpawnEnvironment {
    if row >= UNDERWORLD_START_ROW {
        SpawnEnvironment::Underworld
    } else if row >= CAVERNS_START_ROW {
        SpawnEnvironment::Caverns
    } else if row >= UNDERGROUND_START_ROW {
        SpawnEnvironment::Underground
    } else if is_day {
        SpawnEnvironment::SurfaceDay
    } else {
        SpawnEnvironment::SurfaceNight
    }
}

impl SpawnEnvironment {
    /// `(D, M)`: the 1-in-D per-tick spawn chance and the per-player max
    /// spawns (§5.3 step 1).
    pub fn spawn_params(self) -> (u32, u32) {
        match self {
            SpawnEnvironment::SurfaceDay => (600, 5),
            SpawnEnvironment::SurfaceNight => (300, 7),
            SpawnEnvironment::Underground => (360, 6),
            SpawnEnvironment::Caverns => (240, 8),
            SpawnEnvironment::Underworld => (240, 8),
        }
    }

    /// Species weight table for this environment (§5.3 step 5).
    pub fn species_weights(self) -> &'static [(EnemyKind, u32)] {
        match self {
            SpawnEnvironment::SurfaceDay => {
                &[(EnemyKind::GreenSlime, 60), (EnemyKind::BlueSlime, 40)]
            }
            SpawnEnvironment::SurfaceNight => &[
                (EnemyKind::Zombie, 55),
                (EnemyKind::DemonEye, 35),
                (EnemyKind::BlueSlime, 10),
            ],
            SpawnEnvironment::Underground => {
                &[(EnemyKind::BlueSlime, 60), (EnemyKind::CaveBat, 40)]
            }
            SpawnEnvironment::Caverns => &[
                (EnemyKind::Skeleton, 45),
                (EnemyKind::CaveBat, 40),
                (EnemyKind::BlueSlime, 15),
            ],
            SpawnEnvironment::Underworld => &[
                (EnemyKind::LavaSlime, 50),
                (EnemyKind::AshDemon, 30),
                (EnemyKind::CaveBat, 20),
            ],
        }
    }
}

/// Crowding scaling (§5.3 step 2): with `c` live hostiles of `m` max,
/// multiply D by 0.6/0.7/0.8/0.9/1.0 for c < 20/40/60/80/100% of m.
/// `None` means no spawn (c ≥ m).
pub fn crowding_mult(c: u32, m: u32) -> Option<f32> {
    if c >= m {
        return None;
    }
    let frac = c as f32 / m as f32;
    Some(if frac < 0.2 {
        0.6
    } else if frac < 0.4 {
        0.7
    } else if frac < 0.6 {
        0.8
    } else if frac < 0.8 {
        0.9
    } else {
        1.0
    })
}

/// Spawn-ring extents (§5.3 step 4): candidates lie within ±84 × ±46 tiles
/// of the player but outside the ±62 × ±35 inner (on-screen) rectangle.
pub const SPAWN_RING_OUTER_X: i32 = 84;
pub const SPAWN_RING_OUTER_Y: i32 = 46;
pub const SPAWN_RING_INNER_X: i32 = 62;
pub const SPAWN_RING_INNER_Y: i32 = 35;
/// Candidate tiles tried per successful roll before giving up the tick.
pub const SPAWN_TRIES: u32 = 50;

/// Whether the offset `(dx, dy)` from a player is inside that player's
/// no-spawn (screen) rectangle (§5.3: "never on-screen").
pub fn in_spawn_safe_rect(dx: f32, dy: f32) -> bool {
    dx.abs() < SPAWN_RING_INNER_X as f32 && dy.abs() < SPAWN_RING_INNER_Y as f32
}

/// One uniform candidate offset in the spawn ring: anywhere in the outer
/// rect, rejection-sampled out of the inner rect (the loop terminates with
/// probability 1; the inner rect covers ~56% of the outer).
pub fn spawn_ring_offset(rng: &mut Pcg32) -> (i32, i32) {
    loop {
        let dx = rng.gen_range(-SPAWN_RING_OUTER_X..SPAWN_RING_OUTER_X + 1);
        let dy = rng.gen_range(-SPAWN_RING_OUTER_Y..SPAWN_RING_OUTER_Y + 1);
        if !in_spawn_safe_rect(dx as f32, dy as f32) {
            return (dx, dy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_design_5_1() {
        assert_eq!(ENEMY_DATA.len(), EnemyKind::COUNT);
        assert_eq!(EnemyKind::COUNT, 9);
        let gs = EnemyKind::GreenSlime.data();
        assert_eq!((gs.max_hp, gs.contact_damage, gs.defense), (14, 6, 0));
        assert_eq!(gs.kb_resist, -0.20);
        let z = EnemyKind::Zombie.data();
        assert_eq!((z.max_hp, z.contact_damage, z.defense), (45, 14, 6));
        assert_eq!(z.kb_resist, 0.50);
        assert_eq!(z.coins, 60);
        let ad = EnemyKind::AshDemon.data();
        assert_eq!((ad.max_hp, ad.contact_damage, ad.defense), (120, 32, 8));
        assert_eq!(ad.coins, 300);
        assert_eq!(ad.drops[0].item, ItemId::VoidSickle);
        assert!((ad.drops[0].chance - 0.0286).abs() < 1e-6);
        // Lava slime: 1 SC 20 CC, no gel.
        let ls = EnemyKind::LavaSlime.data();
        assert_eq!(ls.coins, 120);
        assert!(ls.drops.is_empty());
        // Watchling drops nothing and is worth nothing.
        let w = EnemyKind::Watchling.data();
        assert_eq!((w.coins, w.drops.len()), (0, 0));
    }

    #[test]
    fn wire_kind_roundtrips() {
        for &kind in EnemyKind::ALL {
            assert_eq!(EnemyKind::from_wire(kind.wire_kind()), Some(kind));
        }
        assert_eq!(
            EnemyKind::from_wire(crate::protocol::EntityKind::FallingSand),
            None
        );
    }

    #[test]
    fn environment_classification() {
        assert_eq!(spawn_environment(0, true), SpawnEnvironment::SurfaceDay);
        assert_eq!(spawn_environment(340, true), SpawnEnvironment::SurfaceDay);
        assert_eq!(
            spawn_environment(340, false),
            SpawnEnvironment::SurfaceNight
        );
        assert_eq!(spawn_environment(341, true), SpawnEnvironment::Underground);
        assert_eq!(
            spawn_environment(449, false),
            SpawnEnvironment::Underground
        );
        assert_eq!(spawn_environment(450, true), SpawnEnvironment::Caverns);
        assert_eq!(spawn_environment(999, false), SpawnEnvironment::Caverns);
        assert_eq!(spawn_environment(1000, true), SpawnEnvironment::Underworld);
    }

    #[test]
    fn spawn_params_per_design_5_3() {
        assert_eq!(SpawnEnvironment::SurfaceDay.spawn_params(), (600, 5));
        assert_eq!(SpawnEnvironment::SurfaceNight.spawn_params(), (300, 7));
        assert_eq!(SpawnEnvironment::Underground.spawn_params(), (360, 6));
        assert_eq!(SpawnEnvironment::Caverns.spawn_params(), (240, 8));
        assert_eq!(SpawnEnvironment::Underworld.spawn_params(), (240, 8));
    }

    #[test]
    fn crowding_multiplier_boundaries() {
        // M = 10 makes the 20% steps integral.
        assert_eq!(crowding_mult(0, 10), Some(0.6));
        assert_eq!(crowding_mult(1, 10), Some(0.6)); // 10% < 20%
        assert_eq!(crowding_mult(2, 10), Some(0.7)); // exactly 20%
        assert_eq!(crowding_mult(3, 10), Some(0.7));
        assert_eq!(crowding_mult(4, 10), Some(0.8)); // exactly 40%
        assert_eq!(crowding_mult(6, 10), Some(0.9)); // exactly 60%
        assert_eq!(crowding_mult(8, 10), Some(1.0)); // exactly 80%
        assert_eq!(crowding_mult(9, 10), Some(1.0));
        assert_eq!(crowding_mult(10, 10), None); // C ≥ M → no spawn
        assert_eq!(crowding_mult(11, 10), None);
    }

    #[test]
    fn species_weights_match_design() {
        let sum = |env: SpawnEnvironment| -> u32 {
            env.species_weights().iter().map(|&(_, w)| w).sum()
        };
        assert_eq!(sum(SpawnEnvironment::SurfaceDay), 100);
        assert_eq!(sum(SpawnEnvironment::SurfaceNight), 100);
        assert_eq!(sum(SpawnEnvironment::Underground), 100);
        assert_eq!(sum(SpawnEnvironment::Caverns), 100);
        assert_eq!(sum(SpawnEnvironment::Underworld), 100);
        // Watchling never appears in any natural table.
        for env in [
            SpawnEnvironment::SurfaceDay,
            SpawnEnvironment::SurfaceNight,
            SpawnEnvironment::Underground,
            SpawnEnvironment::Caverns,
            SpawnEnvironment::Underworld,
        ] {
            assert!(env
                .species_weights()
                .iter()
                .all(|&(k, _)| k != EnemyKind::Watchling));
        }
    }

    #[test]
    fn spawn_ring_offsets_stay_in_ring() {
        let mut rng = Pcg32::new(5);
        for _ in 0..2_000 {
            let (dx, dy) = spawn_ring_offset(&mut rng);
            assert!(dx.abs() <= SPAWN_RING_OUTER_X && dy.abs() <= SPAWN_RING_OUTER_Y);
            assert!(
                !in_spawn_safe_rect(dx as f32, dy as f32),
                "({dx},{dy}) is on-screen"
            );
        }
    }

    #[test]
    fn coin_variance_stays_in_band() {
        let mut rng = Pcg32::new(11);
        for _ in 0..1_000 {
            let v = coin_drop_value(&mut rng, 100);
            assert!((80..=120).contains(&v), "variance out of band: {v}");
        }
        assert_eq!(coin_drop_value(&mut rng, 0), 0);
    }
}
