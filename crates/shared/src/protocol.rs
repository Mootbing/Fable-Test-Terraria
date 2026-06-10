//! The WebSocket wire protocol: postcard-encoded [`ClientMessage`] /
//! [`ServerMessage`] enums, one per binary frame.
//!
//! # Compatibility rules — read before editing
//!
//! **Every enum in this file is append-only from now on.** postcard encodes
//! variants by index, so reordering, removing, or inserting variants breaks
//! every existing client. New *enum variants* may be appended at the END —
//! that is the only wire-compatible change, because old frames never carry
//! the new index.
//!
//! **Struct fields are positional and NOT extensible**: appending a field to
//! an existing struct or enum-variant payload makes decoders expect bytes
//! that old peers never send (`UnexpectedEnd` → [`decode`] returns `None`).
//! Adding, removing, or reordering fields anywhere is a breaking change and
//! requires a new variant or a [`crate::PROTOCOL_VERSION`] bump so the
//! handshake version gate actually fires. See the `append_compat_rules`
//! test, which pins both behaviors.
//!
//! Authority model (ARCHITECTURE.md): clients send *intents*; the server
//! validates (reach ≤ 6 tiles, item possession, ...) and broadcasts deltas.
//! Own-player movement is the one client-authoritative piece
//! ([`ClientMessage::PlayerState`], sanity-clamped server-side).

use serde::{Deserialize, Serialize};

use crate::items::ItemId;
use crate::tiles::Tile;
use crate::world::WorldFlags;

// Re-exported here because it's a wire type: `{item, count}`, `Option` = empty.
pub use crate::items::InvSlot;

/// Per-player auth token issued by the server on first join and stored in
/// browser localStorage; identifies the player across reconnects.
pub type AuthToken = [u8; 16];

/// Animation flag bits carried in `PlayerState`/`PlayerMoved`.
pub mod anim {
    /// Currently swinging/using the held item.
    pub const USING_ITEM: u8 = 1 << 0;
    /// Standing on ground (false while airborne).
    pub const GROUNDED: u8 = 1 << 1;
    /// Submerged / swim animation.
    pub const IN_LIQUID: u8 = 1 << 2;
}

/// Why an entity left the world ([`ServerMessage::EntityDespawn`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DespawnReason {
    Killed,
    /// Out of range of all players / dawn flee / boss disengage.
    Despawned,
}

/// Every server-simulated entity kind (enemies §5, bosses §6, projectiles,
/// falling tiles). Append-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityKind {
    GreenSlime,
    BlueSlime,
    Zombie,
    DemonEye,
    CaveBat,
    Skeleton,
    LavaSlime,
    AshDemon,
    Watchling,
    SlimeMonarch,
    Watcher,
    BoneWardenSkull,
    BoneWardenHand,
    /// Arrow in flight (the arrow item kind rides in
    /// [`ServerMessage::EntitySpawn`]'s `state` byte, for drop recovery).
    ArrowProjectile,
    FlamingArrowProjectile,
    VoidSickleProjectile,
    FallingSand,
}

/// Town NPC kinds (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NpcKind {
    Sage,
    Merchant,
    Nurse,
}

/// Player debuffs (§8). Append-only. Clients need these: Darkness halves
/// the client-computed light radius (§10), Potion Sickness gates the
/// healing-item UI (§4.4), and the Nurse price preview counts active
/// debuffs (§7.4). Magnitudes/durations live in `shared` constants
/// ([`crate::BURNING_DPS`], [`crate::DARKNESS_LIGHT_RADIUS_MULT`],
/// [`crate::items::POTION_SICKNESS_SECS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Debuff {
    /// 2 dmg/s, ignores defense.
    Burning,
    /// Light radius halved.
    Darkness,
    /// Cannot use another healing item.
    PotionSickness,
}

/// One entry of a [`ServerMessage::PlayerDebuffs`] list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveDebuff {
    pub debuff: Debuff,
    /// Ticks until it wears off.
    pub remaining_ticks: u32,
}

/// One entry of an [`ServerMessage::EntityUpdate`] batch.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EntityState {
    pub id: u32,
    pub pos: (f32, f32),
    pub vel: (f32, f32),
    /// Present when HP changed since the last batch.
    pub hp: Option<u16>,
    /// Kind-specific AI/animation state byte.
    pub state: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NpcInfo {
    pub id: u32,
    pub kind: NpcKind,
    pub name: String,
    pub pos: (f32, f32),
    pub housed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShopEntry {
    pub item: ItemId,
    /// Price in copper coins.
    pub price: u32,
}

/// Client → server. Mostly intents; the server validates everything.
/// **Append-only** (see module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First frame after the socket opens. `token` is `None` on first join;
    /// afterwards the token from [`ServerMessage::Welcome`] reclaims the
    /// persistent player.
    Hello {
        protocol_version: u32,
        name: String,
        token: Option<AuthToken>,
    },
    Ping {
        nonce: u32,
    },
    /// Own-player movement (client-authoritative, ~20/s). `facing` is ±1.
    PlayerState {
        pos: (f32, f32),
        vel: (f32, f32),
        facing: i8,
        anim: u8,
    },
    /// Swing the held tool at a tile (mining damage model §2).
    HitTile {
        x: u32,
        y: u32,
    },
    /// Swing a hammer at the wall layer.
    HitWall {
        x: u32,
        y: u32,
    },
    /// Place the placeable in `hotbar_slot` at (x, y).
    PlaceTile {
        x: u32,
        y: u32,
        hotbar_slot: u8,
    },
    PlaceWall {
        x: u32,
        y: u32,
        hotbar_slot: u8,
    },
    ToggleDoor {
        x: u32,
        y: u32,
    },
    /// Use/consume the item in `slot` (weapon swing, bow shot toward `aim`,
    /// potion drink, summon, warp mirror...). `aim` in world tile coords.
    UseItem {
        slot: u8,
        aim: (f32, f32),
    },
    /// Craft by recipe id (crafting::RECIPES).
    Craft {
        recipe_id: u16,
    },
    /// Move/swap between two inventory slots (flat index, items::inventory).
    MoveSlot {
        from: u8,
        to: u8,
    },
    /// Drop `count` from `slot` onto the ground.
    DropItem {
        slot: u8,
        count: u16,
    },
    /// Open the chest whose origin tile is (x, y).
    OpenChest {
        x: u32,
        y: u32,
    },
    CloseChest,
    /// Move between the open chest and the inventory. `to_chest` gives the
    /// direction; indices are within chest (0..40) / inventory flat array.
    ChestMoveSlot {
        chest_slot: u8,
        inv_slot: u8,
        to_chest: bool,
    },
    TalkNpc {
        npc_id: u32,
    },
    BuyItem {
        npc_id: u32,
        item: ItemId,
        count: u16,
    },
    NurseHeal,
    /// Right-clicked a bed: set personal spawn to its tile coord.
    SetBedSpawn {
        x: u32,
        y: u32,
    },
    Respawn,
    Chat {
        text: String,
    },
    /// Select the held hotbar slot (0–9). Establishes "the held tool" for
    /// `HitTile`/`HitWall`/melee swings and what remote clients render in
    /// this player's hands (rebroadcast as [`ServerMessage::PlayerHeldItem`]).
    SelectSlot {
        slot: u8,
    },
    /// Sell `count` from inventory `slot` to a merchant — §7.3: the merchant
    /// buys back any item at 20% of its base `ItemData::value`. Per-player
    /// transaction (§11).
    SellItem {
        npc_id: u32,
        slot: u8,
        count: u16,
    },
    /// Sleep in the bed at (x, y) (§9: time passes ×5 while all players
    /// sleep, night only). The server validates bed + reach + night.
    Sleep {
        x: u32,
        y: u32,
    },
    /// Get out of bed (also implied by moving/taking damage, server-side).
    WakeUp,
    /// Move half of `from`'s stack (rounded up) onto `to` — the RMB
    /// half-pickup. Same validation as [`ClientMessage::MoveSlot`]; `to`
    /// must be empty or hold the same item.
    SplitSlot {
        from: u8,
        to: u8,
    },
}

/// Server → client. **Append-only** (see module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Handshake accepted.
    Welcome {
        player_id: u32,
        /// Persist this client-side and send in future `Hello`s.
        token: AuthToken,
        world_width: u32,
        world_height: u32,
        spawn: (u32, u32),
        /// Tick of day (`world::DAY_TICKS` cycle) and day count.
        time: u32,
        day: u32,
        flags: WorldFlags,
    },
    /// Handshake rejected (version mismatch, full server, bad name).
    Reject {
        reason: String,
    },
    Pong {
        nonce: u32,
    },
    /// One 64×64 chunk, encoded by `World::encode_chunk` (lz4; decode with
    /// `world::decode_chunk`).
    ChunkData {
        cx: u32,
        cy: u32,
        bytes: Vec<u8>,
    },
    /// Immediate single-cell delta.
    TileChanged {
        x: u32,
        y: u32,
        tile: Tile,
    },
    PlayerJoined {
        id: u32,
        name: String,
        pos: (f32, f32),
    },
    PlayerLeft {
        id: u32,
    },
    /// Another player's movement (interpolate ~100 ms).
    PlayerMoved {
        id: u32,
        pos: (f32, f32),
        vel: (f32, f32),
        facing: i8,
        anim: u8,
    },
    PlayerHealth {
        id: u32,
        hp: u16,
        max_hp: u16,
    },
    PlayerDied {
        id: u32,
    },
    PlayerRespawned {
        id: u32,
        pos: (f32, f32),
    },
    /// `vel` lets fast projectiles (arrows fly at 35 t/s, §4.1) render in
    /// motion immediately instead of stalling until the next `EntityUpdate`
    /// batch (up to 3 ticks / 50 ms later). `state` is the kind-specific
    /// state byte (e.g. the arrow item kind for projectiles).
    EntitySpawn {
        id: u32,
        kind: EntityKind,
        pos: (f32, f32),
        vel: (f32, f32),
        state: u8,
    },
    /// Snapshot batch, broadcast every 3 ticks.
    EntityUpdate {
        entities: Vec<EntityState>,
    },
    EntityDespawn {
        id: u32,
        reason: DespawnReason,
    },
    ItemDropSpawn {
        id: u32,
        item: ItemId,
        count: u16,
        pos: (f32, f32),
        vel: (f32, f32),
    },
    /// `by` is a player id; first pickup wins.
    ItemPickedUp {
        id: u32,
        by: u32,
    },
    /// Full inventory snapshot (flat array, items::inventory layout).
    InventorySync {
        slots: Vec<Option<InvSlot>>,
    },
    /// Single-slot delta.
    SlotChanged {
        idx: u8,
        stack: Option<InvSlot>,
    },
    /// Contents of the chest the player just opened (40 slots).
    ChestContents {
        x: u32,
        y: u32,
        slots: Vec<Option<InvSlot>>,
    },
    /// Chest is locked by another player right now.
    ChestDenied,
    TimeSync {
        time: u32,
        day: u32,
    },
    WorldFlags {
        flags: WorldFlags,
    },
    NpcList {
        npcs: Vec<NpcInfo>,
    },
    NpcDialogue {
        npc_id: u32,
        line: String,
    },
    ShopContents {
        npc_id: u32,
        items: Vec<ShopEntry>,
    },
    Chat {
        from: String,
        text: String,
    },
    /// Server-driven banner text ("You feel something watching you...").
    Toast {
        text: String,
    },
    /// Breath meter (§8, 0..=[`crate::PLAYER_MAX_BREATH`]). Sent to the
    /// owning player while draining/refilling.
    PlayerBreath {
        id: u32,
        breath: u16,
    },
    /// Full replacement list of a player's active debuffs (§8). Broadcast:
    /// remote clients dim Darkness victims' light (§10), the own client
    /// gates healing items on Potion Sickness (§4.4) and previews Nurse
    /// pricing (§7.4).
    PlayerDebuffs {
        id: u32,
        debuffs: Vec<ActiveDebuff>,
    },
    /// Which hotbar slot a player holds and the item in it (`None` = empty
    /// hand). `item` is included because remote clients don't know other
    /// players' inventories.
    PlayerHeldItem {
        id: u32,
        slot: u8,
        item: Option<ItemId>,
    },
    /// Mining progress on a cell (tile or wall layer — clients draw one
    /// crack overlay per cell either way). `damage_frac` is the accumulated
    /// damage as a fraction of the §2 break points, scaled 0–255. Cracks
    /// expire client-side after `tiles::TILE_DAMAGE_RESET_SECS` without a
    /// new frame, mirroring the server's decay, and clear on any
    /// `TileChanged` for the cell.
    BlockCrack {
        x: u32,
        y: u32,
        damage_frac: u8,
    },
    /// Batched cell deltas for world systems that change many cells per tick
    /// (fluid flow, falling sand, grass spread, tree growth). Semantically a
    /// list of [`ServerMessage::TileChanged`]; player-driven single-cell
    /// changes still use the immediate form.
    TilesChanged {
        changes: Vec<(u32, u32, Tile)>,
    },
    /// Single-slot delta of the chest the receiving player has open (the
    /// chest analog of [`ServerMessage::SlotChanged`]). Only the opener
    /// receives these — a chest is locked to one player while open (§11).
    ChestSlotChanged {
        idx: u8,
        stack: Option<InvSlot>,
    },
}

/// Encodes a message into a postcard frame. Infallible for these types
/// (in-memory serialization of derive-only data).
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_allocvec(msg).expect("postcard encode cannot fail for our types")
}

/// Decodes a postcard frame; `None` on any malformed input.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    postcard::from_bytes(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::{state, Liquid, LiquidKind, TileId, WallId};
    use crate::world::World;

    fn roundtrip_client(msg: ClientMessage) {
        let bytes = encode(&msg);
        let back: ClientMessage = decode(&bytes).expect("decode");
        assert_eq!(back, msg);
    }

    fn roundtrip_server(msg: ServerMessage) {
        let bytes = encode(&msg);
        let back: ServerMessage = decode(&bytes).expect("decode");
        assert_eq!(back, msg);
    }

    #[test]
    fn client_messages_roundtrip() {
        roundtrip_client(ClientMessage::Hello {
            protocol_version: crate::PROTOCOL_VERSION,
            name: "moo".into(),
            token: Some([7; 16]),
        });
        roundtrip_client(ClientMessage::Hello {
            protocol_version: 1,
            name: "first-join".into(),
            token: None,
        });
        roundtrip_client(ClientMessage::PlayerState {
            pos: (2100.5, 280.25),
            vel: (-11.25, 37.5),
            facing: -1,
            anim: anim::USING_ITEM | anim::GROUNDED,
        });
        roundtrip_client(ClientMessage::HitTile { x: 4199, y: 1199 });
        roundtrip_client(ClientMessage::PlaceTile {
            x: 10,
            y: 20,
            hotbar_slot: 9,
        });
        roundtrip_client(ClientMessage::UseItem {
            slot: 3,
            aim: (1.5, -2.5),
        });
        roundtrip_client(ClientMessage::Craft { recipe_id: 70 });
        roundtrip_client(ClientMessage::ChestMoveSlot {
            chest_slot: 39,
            inv_slot: 56,
            to_chest: false,
        });
        roundtrip_client(ClientMessage::BuyItem {
            npc_id: 2,
            item: ItemId::MiningHelmet,
            count: 1,
        });
        roundtrip_client(ClientMessage::Chat {
            text: "hello world".into(),
        });
        roundtrip_client(ClientMessage::SelectSlot { slot: 9 });
        roundtrip_client(ClientMessage::SellItem {
            npc_id: 2,
            slot: 14,
            count: 30,
        });
        roundtrip_client(ClientMessage::Sleep { x: 2105, y: 277 });
        roundtrip_client(ClientMessage::WakeUp);
        roundtrip_client(ClientMessage::MoveSlot { from: 0, to: 56 });
        roundtrip_client(ClientMessage::SplitSlot { from: 9, to: 49 });
        roundtrip_client(ClientMessage::DropItem { slot: 12, count: 999 });
        roundtrip_client(ClientMessage::OpenChest { x: 2100, y: 950 });
        roundtrip_client(ClientMessage::CloseChest);
    }

    #[test]
    fn server_messages_roundtrip() {
        roundtrip_server(ServerMessage::Welcome {
            player_id: 1,
            token: [0xab; 16],
            world_width: 4200,
            world_height: 1200,
            spawn: (2100, 279),
            time: crate::world::NEW_WORLD_TIME,
            day: 3,
            flags: WorldFlags {
                watcher_defeated: true,
                ..WorldFlags::default()
            },
        });
        roundtrip_server(ServerMessage::TileChanged {
            x: 100,
            y: 200,
            tile: Tile {
                id: TileId::Door,
                wall: WallId::Wood,
                liquid: Liquid::new(LiquidKind::Water, 3),
                state: state::DOOR_OPEN,
            },
        });
        roundtrip_server(ServerMessage::EntityUpdate {
            entities: vec![
                EntityState {
                    id: 9,
                    pos: (1.0, 2.0),
                    vel: (0.5, -0.5),
                    hp: Some(14),
                    state: 2,
                },
                EntityState {
                    id: 10,
                    pos: (3.0, 4.0),
                    vel: (0.0, 0.0),
                    hp: None,
                    state: 0,
                },
            ],
        });
        roundtrip_server(ServerMessage::EntitySpawn {
            id: 77,
            kind: EntityKind::BoneWardenSkull,
            pos: (2100.0, 1100.0),
            vel: (0.0, -10.0),
            state: 0,
        });
        roundtrip_server(ServerMessage::EntitySpawn {
            id: 78,
            kind: EntityKind::ArrowProjectile,
            pos: (10.0, 20.0),
            vel: (35.0, 0.0),
            state: ItemId::WoodenArrow as u8,
        });
        roundtrip_server(ServerMessage::EntityDespawn {
            id: 77,
            reason: DespawnReason::Killed,
        });
        roundtrip_server(ServerMessage::InventorySync {
            slots: vec![
                Some(InvSlot::new(ItemId::WoodPickaxe, 1)),
                None,
                Some(InvSlot::new(ItemId::Torch, 99)),
            ],
        });
        roundtrip_server(ServerMessage::ShopContents {
            npc_id: 1,
            items: vec![ShopEntry {
                item: ItemId::Torch,
                price: 50,
            }],
        });
        roundtrip_server(ServerMessage::Toast {
            text: "You feel something watching you...".into(),
        });
        roundtrip_server(ServerMessage::PlayerBreath { id: 1, breath: 137 });
        roundtrip_server(ServerMessage::PlayerDebuffs {
            id: 1,
            debuffs: vec![
                ActiveDebuff {
                    debuff: Debuff::Burning,
                    remaining_ticks: 7 * crate::TICK_RATE,
                },
                ActiveDebuff {
                    debuff: Debuff::PotionSickness,
                    remaining_ticks: 60 * crate::TICK_RATE,
                },
            ],
        });
        roundtrip_server(ServerMessage::PlayerDebuffs {
            id: 2,
            debuffs: vec![], // all cleared
        });
        roundtrip_server(ServerMessage::PlayerHeldItem {
            id: 1,
            slot: 0,
            item: Some(ItemId::WoodSword),
        });
        roundtrip_server(ServerMessage::PlayerHeldItem {
            id: 1,
            slot: 4,
            item: None,
        });
        roundtrip_server(ServerMessage::BlockCrack {
            x: 4199,
            y: 1199,
            damage_frac: 255,
        });
        roundtrip_server(ServerMessage::TilesChanged {
            changes: vec![
                (10, 20, Tile::of(TileId::Sand)),
                (
                    10,
                    21,
                    Tile {
                        id: TileId::Air,
                        wall: WallId::Stone,
                        liquid: Liquid::new(LiquidKind::Water, 1),
                        state: 0,
                    },
                ),
            ],
        });
        roundtrip_server(ServerMessage::SlotChanged {
            idx: 52,
            stack: Some(InvSlot::new(ItemId::GoldGreaves, 1)),
        });
        roundtrip_server(ServerMessage::SlotChanged {
            idx: 0,
            stack: None,
        });
        roundtrip_server(ServerMessage::ChestContents {
            x: 17,
            y: 902,
            slots: vec![Some(InvSlot::new(ItemId::SwiftBoots, 1)), None],
        });
        roundtrip_server(ServerMessage::ChestDenied);
        roundtrip_server(ServerMessage::ChestSlotChanged {
            idx: 39,
            stack: Some(InvSlot::new(ItemId::GoldBar, 8)),
        });
        roundtrip_server(ServerMessage::ChestSlotChanged {
            idx: 0,
            stack: None,
        });
    }

    /// Pins the module-doc compatibility rules against postcard itself:
    /// appending an enum *variant* is wire-compatible; appending a *field*
    /// to an existing struct/variant payload is breaking.
    #[test]
    fn append_compat_rules() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        enum V1 {
            A { x: u32 },
            B,
        }
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        enum V2 {
            A { x: u32 },
            B,
            C { y: u8 }, // appended variant
        }
        // Old peer's frame decodes fine on the extended enum...
        let old_frame = encode(&V1::A { x: 7 });
        assert_eq!(decode::<V2>(&old_frame), Some(V2::A { x: 7 }));
        // ...but a frame of the new variant is garbage to the old peer.
        let new_frame = encode(&V2::C { y: 1 });
        assert_eq!(decode::<V1>(&new_frame), None);

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct S1 {
            a: u32,
        }
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct S2 {
            a: u32,
            b: u8, // appended field — NOT compatible
        }
        // Fields are positional: the extended decoder hits UnexpectedEnd on
        // an old frame. Appending fields requires a PROTOCOL_VERSION bump.
        let old_frame = encode(&S1 { a: 7 });
        assert_eq!(decode::<S2>(&old_frame), None);
    }

    #[test]
    fn chunk_data_roundtrips_through_protocol() {
        let mut w = World::new(80, 80);
        w.set_tile(5, 5, Tile::of(TileId::Hellstone));
        w.set_tile(63, 63, Tile::of(TileId::Obsidian));
        let msg = ServerMessage::ChunkData {
            cx: 0,
            cy: 0,
            bytes: w.encode_chunk(0, 0),
        };
        let bytes = encode(&msg);
        let back: ServerMessage = decode(&bytes).expect("decode");
        let ServerMessage::ChunkData { cx, cy, bytes } = back else {
            panic!("wrong variant");
        };
        assert_eq!((cx, cy), (0, 0));
        let tiles = crate::world::decode_chunk(&bytes).expect("chunk");
        assert_eq!(tiles[5 * 64 + 5].id, TileId::Hellstone);
        assert_eq!(tiles[63 * 64 + 63].id, TileId::Obsidian);
    }

    #[test]
    fn malformed_frames_decode_to_none() {
        assert_eq!(decode::<ServerMessage>(&[0xff, 0xff, 0xff, 0xff]), None);
        assert_eq!(decode::<ClientMessage>(&[]), None);
    }
}
