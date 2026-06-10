//! The WebSocket wire protocol: postcard-encoded [`ClientMessage`] /
//! [`ServerMessage`] enums, one per binary frame.
//!
//! # Compatibility rules — read before editing
//!
//! **Every enum in this file is append-only from now on.** postcard encodes
//! variants by index, so reordering, removing, or inserting variants (or
//! struct fields) breaks every existing client. Add new variants/fields at
//! the END only, and bump [`crate::PROTOCOL_VERSION`] for any breaking
//! change.
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
    /// Arrow in flight (item kind rides in the spawn's `state`/drop).
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
    EntitySpawn {
        id: u32,
        kind: EntityKind,
        pos: (f32, f32),
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
