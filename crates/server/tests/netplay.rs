//! End-to-end netplay test: boots the real server (axum + sim task) on an
//! ephemeral port with a small generated world and drives it with real
//! WebSocket clients (tokio-tungstenite).

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use ferraria_shared::items::{inventory, InvSlot, ItemId, STARTING_KIT};
use ferraria_shared::protocol::{decode, encode, ClientMessage, ServerMessage};
use ferraria_shared::tiles::TileId;
use ferraria_shared::world::CHEST_SLOTS;
use ferraria_shared::{CHAT_MAX_CHARS, PROTOCOL_VERSION};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
/// Small but above the worldgen minimum (300×300); generates in well under
/// a second even in debug builds.
const TEST_WORLD: (u64, u32, u32) = (7, 300, 300);

/// Boots a server on an ephemeral port; returns the port.
async fn start_server() -> u16 {
    start_server_with(|_| {}).await
}

/// Boots a server whose generated world is first adjusted by `prepare`
/// (placing test furniture, filling chests, ...).
async fn start_server_with(prepare: impl FnOnce(&mut ferraria_shared::world::World)) -> u16 {
    let (seed, w, h) = TEST_WORLD;
    let mut world = ferraria_server::worldgen::generate_with_size(seed, w, h);
    prepare(&mut world);
    let app = ferraria_server::net::router(world, seed, "web");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server runs");
    });
    port
}

async fn connect(port: u16) -> Ws {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws"))
        .await
        .expect("ws connect");
    ws
}

async fn send(ws: &mut Ws, msg: &ClientMessage) {
    ws.send(Message::Binary(encode(msg).into()))
        .await
        .expect("ws send");
}

/// Next binary frame, decoded; panics on timeout/close.
async fn recv(ws: &mut Ws) -> ServerMessage {
    loop {
        let frame = tokio::time::timeout(RECV_TIMEOUT, ws.next())
            .await
            .expect("server replied within timeout")
            .expect("socket still open")
            .expect("clean frame");
        if let Message::Binary(bytes) = frame {
            return decode::<ServerMessage>(&bytes).expect("decodable server frame");
        }
    }
}

/// Skips frames (TimeSync etc. interleave freely) until `pick` matches.
async fn expect<T>(ws: &mut Ws, what: &str, mut pick: impl FnMut(ServerMessage) -> Option<T>) -> T {
    for _ in 0..200 {
        if let Some(out) = pick(recv(ws).await) {
            return out;
        }
    }
    panic!("never received {what}");
}

/// Hello → Welcome; returns the assigned player id and world spawn.
async fn join(ws: &mut Ws, name: &str) -> (u32, (u32, u32)) {
    send(
        ws,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            name: name.into(),
            token: None,
        },
    )
    .await;
    match recv(ws).await {
        ServerMessage::Welcome {
            player_id, spawn, ..
        } => (player_id, spawn),
        other => panic!("expected Welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn connect_spawn_walk_chat_leave() {
    let port = start_server().await;

    // ---- A joins: Welcome, then chunks + InventorySync + TimeSync. --------
    let mut a = connect(port).await;
    send(
        &mut a,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            name: "alice".into(),
            token: None,
        },
    )
    .await;
    let welcome = recv(&mut a).await;
    let ServerMessage::Welcome {
        player_id: a_id,
        world_width,
        world_height,
        spawn,
        ..
    } = welcome
    else {
        panic!("expected Welcome, got {welcome:?}");
    };
    assert_eq!((world_width, world_height), (TEST_WORLD.1, TEST_WORLD.2));

    let mut chunks = 0u32;
    let mut inv: Option<Vec<Option<InvSlot>>> = None;
    let mut time = false;
    let mut own_pos: Option<(f32, f32)> = None;
    while chunks == 0 || inv.is_none() || !time {
        match recv(&mut a).await {
            ServerMessage::ChunkData { bytes, .. } => {
                ferraria_shared::world::decode_chunk(&bytes).expect("chunk decodes");
                chunks += 1;
            }
            ServerMessage::InventorySync { slots } => inv = Some(slots),
            ServerMessage::TimeSync { .. } => time = true,
            // Authoritative own placement (used by reconnect reclaim).
            ServerMessage::PlayerMoved { id, pos, .. } if id == a_id => own_pos = Some(pos),
            // §8 vitals arrive with the join state too.
            ServerMessage::PlayerHealth { id, hp, max_hp } if id == a_id => {
                assert_eq!((hp, max_hp), (100, 100), "fresh §8 HP");
            }
            other => panic!("unexpected join frame {other:?}"),
        }
    }
    let own_pos = own_pos.expect("own placement frame sent with the join state");
    assert!(
        (own_pos.0 - spawn.0 as f32).abs() < 3.0 && (own_pos.1 - spawn.1 as f32).abs() < 4.0,
        "fresh join placement is at the world spawn {spawn:?}, got {own_pos:?}"
    );
    let inv = inv.expect("inventory synced");
    assert_eq!(inv.len(), inventory::TOTAL);
    for (i, kit) in STARTING_KIT.iter().enumerate() {
        assert_eq!(inv[i], Some(*kit), "starting kit slot {i}");
    }
    assert_eq!(inv[STARTING_KIT.len()], None);

    // ---- B joins: A hears about it. ----------------------------------------
    let mut b = connect(port).await;
    let (b_id, _) = join(&mut b, "bob").await;
    assert_ne!(a_id, b_id);
    let (joined_name, joined_pos) = expect(&mut a, "PlayerJoined", |m| match m {
        ServerMessage::PlayerJoined { id, name, pos } if id == b_id => Some((name, pos)),
        _ => None,
    })
    .await;
    assert_eq!(joined_name, "bob");
    assert!(
        (joined_pos.0 - spawn.0 as f32).abs() < 3.0 && (joined_pos.1 - spawn.1 as f32).abs() < 4.0,
        "bob spawned at the world spawn {spawn:?}, got {joined_pos:?}"
    );

    // ---- B walks; A sees PlayerMoved with the same positions. --------------
    let targets = [
        (joined_pos.0 + 2.0, joined_pos.1 - 1.0),
        (joined_pos.0 + 4.5, joined_pos.1 - 1.5),
    ];
    for &target in &targets {
        send(
            &mut b,
            &ClientMessage::PlayerState {
                pos: target,
                vel: (11.25, 0.0),
                facing: 1,
                anim: 0,
            },
        )
        .await;
        let moved = expect(&mut a, "PlayerMoved", |m| match m {
            ServerMessage::PlayerMoved { id, pos, .. } if id == b_id => Some(pos),
            _ => None,
        })
        .await;
        assert_eq!(moved, target, "rebroadcast position matches");
    }

    // ---- Chat: oversized message arrives truncated. -------------------------
    let long: String = "x".repeat(CHAT_MAX_CHARS + 50);
    send(&mut b, &ClientMessage::Chat { text: long }).await;
    let chat = expect(&mut a, "Chat", |m| match m {
        ServerMessage::Chat { from, text } => Some((from, text)),
        _ => None,
    })
    .await;
    assert_eq!(chat.0, "bob");
    assert_eq!(chat.1.chars().count(), CHAT_MAX_CHARS, "chat capped");

    // ---- B leaves; A hears PlayerLeft. --------------------------------------
    b.close(None).await.expect("close b");
    expect(&mut a, "PlayerLeft", |m| match m {
        ServerMessage::PlayerLeft { id } if id == b_id => Some(()),
        _ => None,
    })
    .await;
}

/// Regression for the reconnect-reclaim desync: the server restores the old
/// position on a token reclaim and must push it to the client as an own-id
/// `PlayerMoved` in the join state — otherwise the client predicts from the
/// world spawn (frozen forever if the spawn chunk isn't streamed).
#[tokio::test]
async fn token_reclaim_restores_position_after_reconnect() {
    let port = start_server().await;
    let mut ws = connect(port).await;
    send(
        &mut ws,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            name: "carol".into(),
            token: None,
        },
    )
    .await;
    let (player_id, token, spawn) = match recv(&mut ws).await {
        ServerMessage::Welcome {
            player_id,
            token,
            spawn,
            ..
        } => (player_id, token, spawn),
        other => panic!("expected Welcome, got {other:?}"),
    };

    // Walk a few tiles away, then use a Ping barrier to be sure the sim has
    // applied the movement (commands are processed in order).
    let dest = (spawn.0 as f32 + 5.0, spawn.1 as f32 - 3.0);
    send(
        &mut ws,
        &ClientMessage::PlayerState {
            pos: dest,
            vel: (0.0, 0.0),
            facing: 1,
            anim: 0,
        },
    )
    .await;
    send(&mut ws, &ClientMessage::Ping { nonce: 7 }).await;
    expect(&mut ws, "Pong", |m| match m {
        ServerMessage::Pong { nonce: 7 } => Some(()),
        _ => None,
    })
    .await;
    ws.close(None).await.expect("close");

    // Wait for the server to process the disconnect (frees the name).
    let mut freed = false;
    for _ in 0..50 {
        if http_get(port, "/api/status")
            .await
            .contains("\"players\":0")
        {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(freed, "session released after close");

    // Reconnect with the token: same id, and the join state must carry the
    // reclaimed position as an own-id PlayerMoved.
    let mut ws2 = connect(port).await;
    send(
        &mut ws2,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            name: "carol".into(),
            token: Some(token),
        },
    )
    .await;
    match recv(&mut ws2).await {
        ServerMessage::Welcome { player_id: id, .. } => {
            assert_eq!(id, player_id, "identity reclaimed")
        }
        other => panic!("expected Welcome, got {other:?}"),
    }
    let restored = expect(&mut ws2, "own-id placement", |m| match m {
        ServerMessage::PlayerMoved { id, pos, .. } if id == player_id => Some(pos),
        _ => None,
    })
    .await;
    assert_eq!(restored, dest, "reclaimed position pushed to the client");
}

/// The full mine → drop → pickup loop over a real WebSocket: select the
/// pickaxe, swing at the grass platform under the spawn at the §4.1 swing
/// cadence (watching `BlockCrack` progress), see the broken tile and the
/// `ItemDropSpawn`, then collect it (`ItemPickedUp` + `SlotChanged`).
#[tokio::test]
async fn mine_drop_and_pickup_over_websocket() {
    let port = start_server().await;
    let mut ws = connect(port).await;
    let (player_id, spawn) = join(&mut ws, "miner").await;

    // Hold the starting-kit Wood Pickaxe (hotbar slot 1).
    send(&mut ws, &ClientMessage::SelectSlot { slot: 1 }).await;

    // The tile under the spawn platform's center is worldgen grass (soft ×2
    // vs the pick's 25 power → 2 swings), and the player stands right on
    // top of it, well inside reach and pickup range.
    let (tx, ty) = (spawn.0, spawn.1 + 1);
    let mut broke = false;
    let mut saw_crack = false;
    let mut drop: Option<(u32, ferraria_shared::items::ItemId)> = None;
    'mining: for _ in 0..10 {
        send(&mut ws, &ClientMessage::HitTile { x: tx, y: ty }).await;
        // Swing cadence: the wood pickaxe needs 0.30 s between swings; the
        // server drops faster spam.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
        while tokio::time::Instant::now() < deadline {
            let Ok(frame) = tokio::time::timeout_at(deadline, ws.next()).await else {
                break; // cadence pause elapsed: swing again
            };
            let frame = frame.expect("socket open").expect("clean frame");
            let Message::Binary(bytes) = frame else {
                continue;
            };
            match decode::<ServerMessage>(&bytes).expect("decodable") {
                ServerMessage::BlockCrack { x, y, damage_frac } if (x, y) == (tx, ty) => {
                    assert!(damage_frac > 0);
                    saw_crack = true;
                }
                ServerMessage::TileChanged { x, y, tile } if (x, y) == (tx, ty) => {
                    assert_eq!(tile.id, ferraria_shared::tiles::TileId::Air);
                    broke = true;
                }
                ServerMessage::ItemDropSpawn {
                    id,
                    item,
                    count,
                    pos,
                    ..
                } => {
                    assert!(count >= 1);
                    assert!(
                        (pos.0 - tx as f32).abs() < 2.0 && (pos.1 - ty as f32).abs() < 2.0,
                        "drop spawns at the broken tile, got {pos:?}"
                    );
                    drop = Some((id, item));
                }
                _ => {}
            }
            if broke && drop.is_some() {
                break 'mining;
            }
        }
    }
    assert!(saw_crack, "mining progress was broadcast as BlockCrack");
    assert!(broke, "the tile broke within 10 swings");
    let (drop_id, drop_item) = drop.expect("breaking spawned an item drop");

    // Stand still next to the hole: after the 0.5 s arming delay the server
    // auto-collects the drop into our inventory. The owner's SlotChanged is
    // queued just before the ItemPickedUp broadcast.
    let stack = expect(&mut ws, "SlotChanged", |m| match m {
        ServerMessage::SlotChanged { stack: Some(s), .. } if s.item == drop_item => Some(s),
        _ => None,
    })
    .await;
    assert!(stack.count >= 1);
    let picked_by = expect(&mut ws, "ItemPickedUp", |m| match m {
        ServerMessage::ItemPickedUp { id, by } if id == drop_id => Some(by),
        _ => None,
    })
    .await;
    assert_eq!(picked_by, player_id, "we collected our own drop");
}

/// End-to-end inventory/chest/craft flow: a chest near spawn is locked to
/// its opener (second client gets `ChestDenied`), chest→inventory moves and
/// a websocket `Craft` mutate the inventory through observed `SlotChanged`
/// deltas, and closing hands the chest over.
#[tokio::test]
async fn chest_lock_and_crafting_over_websocket() {
    // Stock a chest near the world spawn with the wood the craft will use.
    let mut chest_pos = (0u32, 0u32);
    let port = start_server_with(|world| {
        let (sx, sy) = world.spawn;
        // Keep candidates within REACH (6) of the spawn-standing player.
        let mut placed = None;
        'search: for dx in 2..5u32 {
            for dy in 1..4u32 {
                let (x, y) = (sx + dx, sy.saturating_sub(dy));
                if world.place_multitile(x, y, TileId::Chest) {
                    placed = Some((x, y));
                    break 'search;
                }
            }
        }
        let (x, y) = placed.expect("a free 2x2 spot near spawn for the chest");
        let mut slots = vec![None; CHEST_SLOTS];
        slots[0] = Some(InvSlot::new(ItemId::Wood, 10));
        world.chests.insert((x, y), slots);
        chest_pos = (x, y);
    })
    .await;
    let (cx, cy) = chest_pos;

    let mut a = connect(port).await;
    let _ = join(&mut a, "alice").await;
    let mut b = connect(port).await;
    let _ = join(&mut b, "bob").await;

    // ---- Alice opens the chest and sees the stocked wood. -------------------
    send(&mut a, &ClientMessage::OpenChest { x: cx, y: cy }).await;
    let slots = expect(&mut a, "ChestContents", |m| match m {
        ServerMessage::ChestContents { x, y, slots } if (x, y) == (cx, cy) => Some(slots),
        _ => None,
    })
    .await;
    assert_eq!(slots.len(), CHEST_SLOTS);
    assert_eq!(slots[0], Some(InvSlot::new(ItemId::Wood, 10)));

    // ---- Bob is denied while Alice holds it open. ----------------------------
    send(&mut b, &ClientMessage::OpenChest { x: cx, y: cy }).await;
    expect(&mut b, "ChestDenied", |m| match m {
        ServerMessage::ChestDenied => Some(()),
        _ => None,
    })
    .await;

    // ---- Alice withdraws the wood: both sides' deltas observed. --------------
    send(
        &mut a,
        &ClientMessage::ChestMoveSlot {
            chest_slot: 0,
            inv_slot: 4,
            to_chest: false,
        },
    )
    .await;
    let (mut got_inv, mut got_chest) = (false, false);
    while !(got_inv && got_chest) {
        match recv(&mut a).await {
            ServerMessage::SlotChanged { idx: 4, stack } => {
                assert_eq!(stack, Some(InvSlot::new(ItemId::Wood, 10)));
                got_inv = true;
            }
            ServerMessage::ChestSlotChanged { idx: 0, stack } => {
                assert_eq!(stack, None);
                got_chest = true;
            }
            _ => {}
        }
    }

    // ---- Craft a workbench (recipe 1, Hands): 10 wood -> 1 workbench. --------
    send(&mut a, &ClientMessage::Craft { recipe_id: 1 }).await;
    let crafted = expect(&mut a, "SlotChanged for the craft", |m| match m {
        ServerMessage::SlotChanged { idx: 4, stack } => Some(stack),
        _ => None,
    })
    .await;
    assert_eq!(
        crafted,
        Some(InvSlot::new(ItemId::Workbench, 1)),
        "wood consumed, workbench in its place"
    );

    // ---- Close: Bob can now take over the (emptied) chest. -------------------
    send(&mut a, &ClientMessage::CloseChest).await;
    send(&mut a, &ClientMessage::Ping { nonce: 1 }).await; // order barrier
    expect(&mut a, "Pong", |m| match m {
        ServerMessage::Pong { nonce: 1 } => Some(()),
        _ => None,
    })
    .await;
    send(&mut b, &ClientMessage::OpenChest { x: cx, y: cy }).await;
    let slots = expect(&mut b, "ChestContents after takeover", |m| match m {
        ServerMessage::ChestContents { x, y, slots } if (x, y) == (cx, cy) => Some(slots),
        _ => None,
    })
    .await;
    assert_eq!(slots[0], None, "alice took the wood");
}

/// Town NPCs end to end (DESIGN §7): the boot-time Sage answers `TalkNpc`
/// with a §7.5 line; stocking up 50 SC from a chest trips the hourly
/// Merchant arrival check (a valid house is pre-built), and `BuyItem`
/// round-trips a purchase with coin change.
#[tokio::test]
async fn talk_to_sage_and_buy_from_merchant_over_websocket() {
    use ferraria_server::sim::npc::IN_GAME_HOUR_TICKS;
    use ferraria_shared::npc::{npc_data, MERCHANT_SHOP};
    use ferraria_shared::physics::PLAYER_HEIGHT;
    use ferraria_shared::protocol::NpcKind;
    use ferraria_shared::tiles::{state, Tile, WallId};

    let mut chest_pos = (0u32, 0u32);
    let mut house_x0 = 0u32;
    let mut floor_y = 0u32;
    let port = start_server_with(|world| {
        let (sx, sy) = world.spawn;
        // ---- A §7.1-valid house: 10×4 interior, stone shell, wood walls,
        // door, torch, table, chair — stamped over whatever terrain is
        // there, 8 tiles right of spawn.
        house_x0 = sx + 8;
        floor_y = sy + 1; // the worldgen spawn platform row
        let (x0, ih) = (house_x0, 4u32);
        let y_top = floor_y - ih;
        for x in x0 - 1..=x0 + 10 {
            for y in y_top - 1..=floor_y {
                let interior = (x0..x0 + 10).contains(&x) && (y_top..floor_y).contains(&y);
                let mut t = Tile::AIR;
                if interior {
                    t.wall = WallId::Wood;
                } else {
                    t.id = TileId::Stone;
                }
                world.set_tile(x, y, t);
            }
        }
        for d in 0..3u32 {
            let mut t = world.tile(x0 - 1, floor_y - 1 - d);
            t.id = TileId::Door;
            t.state = state::part(0, (2 - d) as u8);
            world.set_tile(x0 - 1, floor_y - 1 - d, t);
        }
        let mut torch = world.tile(x0, floor_y - 1);
        torch.id = TileId::Torch;
        world.set_tile(x0, floor_y - 1, torch);
        assert!(world.place_multitile(x0 + 1, floor_y - 2, TileId::Table));
        assert!(world.place_multitile(x0 + 4, floor_y - 2, TileId::Chair));

        // ---- A chest by the spawn holding the 50 SC arrival threshold.
        let mut placed = None;
        'search: for dx in 2..5u32 {
            for dy in 1..4u32 {
                let (x, y) = (sx - dx, sy.saturating_sub(dy));
                if world.place_multitile(x, y, TileId::Chest) {
                    placed = Some((x, y));
                    break 'search;
                }
            }
        }
        let (cx, cy) = placed.expect("a chest spot near spawn");
        let mut slots = vec![None; CHEST_SLOTS];
        slots[0] = Some(InvSlot::new(ItemId::SilverCoin, 50));
        world.chests.insert((cx, cy), slots);
        chest_pos = (cx, cy);

        // ---- Park the clock 5 real seconds before an in-game-hour
        // boundary, so the §7.2 arrival check fires shortly after boot.
        world.time = (world.time / IN_GAME_HOUR_TICKS + 1) * IN_GAME_HOUR_TICKS - 300;
    })
    .await;

    let mut ws = connect(port).await;
    let _ = join(&mut ws, "trader").await;

    // ---- The boot-time Sage is in the join-state roster. -----------------
    let sage = expect(&mut ws, "NpcList with the Sage", |m| match m {
        ServerMessage::NpcList { npcs } => npcs
            .iter()
            .find(|n| n.kind == NpcKind::Sage)
            .map(|n| (n.id, n.name.clone())),
        _ => None,
    })
    .await;
    assert_eq!(sage.1, "Sage the Guide");

    // ---- Talk to the Sage: a §7.5 line comes back. -------------------------
    send(&mut ws, &ClientMessage::TalkNpc { npc_id: sage.0 }).await;
    let line = expect(&mut ws, "NpcDialogue", |m| match m {
        ServerMessage::NpcDialogue { npc_id, line } if npc_id == sage.0 => Some(line),
        _ => None,
    })
    .await;
    assert!(
        npc_data(NpcKind::Sage).lines.iter().any(|l| l.text == line),
        "a verbatim §7.5 Sage line, got {line:?}"
    );

    // ---- Withdraw the 50 SC from the chest. ---------------------------------
    let (cx, cy) = chest_pos;
    send(&mut ws, &ClientMessage::OpenChest { x: cx, y: cy }).await;
    expect(&mut ws, "ChestContents", |m| match m {
        ServerMessage::ChestContents { .. } => Some(()),
        _ => None,
    })
    .await;
    send(
        &mut ws,
        &ClientMessage::ChestMoveSlot {
            chest_slot: 0,
            inv_slot: 9,
            to_chest: false,
        },
    )
    .await;
    expect(&mut ws, "coins in slot 9", |m| match m {
        ServerMessage::SlotChanged { idx: 9, stack } => {
            assert_eq!(stack, Some(InvSlot::new(ItemId::SilverCoin, 50)));
            Some(())
        }
        _ => None,
    })
    .await;

    // ---- The hourly arrival check trips: toast + roster update. -------------
    expect(&mut ws, "merchant arrival toast", |m| match m {
        ServerMessage::Toast { text } if text == "The Merchant has arrived!" => Some(()),
        _ => None,
    })
    .await;
    let merchant = expect(&mut ws, "NpcList with the Merchant", |m| match m {
        ServerMessage::NpcList { npcs } => npcs
            .iter()
            .find(|n| n.kind == NpcKind::Merchant && n.housed)
            .map(|n| n.id),
        _ => None,
    })
    .await;

    // ---- Walk into the house, talk (catalog comes along), and buy. -----------
    let home_x = house_x0 as f32 + 6.0;
    send(
        &mut ws,
        &ClientMessage::PlayerState {
            pos: (home_x, floor_y as f32 - PLAYER_HEIGHT - 0.01),
            vel: (0.0, 0.0),
            facing: 1,
            anim: 0,
        },
    )
    .await;
    send(&mut ws, &ClientMessage::TalkNpc { npc_id: merchant }).await;
    let catalog = expect(&mut ws, "ShopContents", |m| match m {
        ServerMessage::ShopContents { npc_id, items } if npc_id == merchant => Some(items),
        _ => None,
    })
    .await;
    assert_eq!(catalog.len(), MERCHANT_SHOP.len());

    send(
        &mut ws,
        &ClientMessage::BuyItem {
            npc_id: merchant,
            item: ItemId::Bottle,
            count: 1,
        },
    )
    .await;
    // The purchase lands in the inventory and the coins make change
    // (50 SC − 20 CC → a broken silver).
    let (mut got_bottle, mut got_change) = (false, false);
    while !(got_bottle && got_change) {
        match recv(&mut ws).await {
            ServerMessage::SlotChanged { stack: Some(s), .. } if s.item == ItemId::Bottle => {
                assert_eq!(s.count, 1);
                got_bottle = true;
            }
            ServerMessage::SlotChanged { stack: Some(s), .. } if s.item == ItemId::CopperCoin => {
                assert_eq!(s.count, 80, "change from the broken silver");
                got_change = true;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn wrong_protocol_version_is_rejected() {
    let port = start_server().await;
    let mut ws = connect(port).await;
    send(
        &mut ws,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            name: "future-client".into(),
            token: None,
        },
    )
    .await;
    let reply = recv(&mut ws).await;
    let ServerMessage::Reject { reason } = reply else {
        panic!("expected Reject, got {reply:?}");
    };
    assert!(
        reason.contains("version"),
        "reason mentions the version mismatch: {reason}"
    );
}

#[tokio::test]
async fn duplicate_name_is_rejected() {
    let port = start_server().await;
    let mut first = connect(port).await;
    let _ = join(&mut first, "alice").await;
    let mut second = connect(port).await;
    send(
        &mut second,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            name: "alice".into(),
            token: None,
        },
    )
    .await;
    let reply = recv(&mut second).await;
    assert!(
        matches!(reply, ServerMessage::Reject { .. }),
        "expected Reject, got {reply:?}"
    );
}

#[tokio::test]
async fn status_endpoint_reports_players_and_seed() {
    let port = start_server().await;
    let mut ws = connect(port).await;
    let _ = join(&mut ws, "alice").await;
    // Poll until the (async) join is visible.
    let mut body = String::new();
    for _ in 0..50 {
        body = http_get(port, "/api/status").await;
        if body.contains("\"players\":1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(body.contains("\"players\":1"), "status body: {body}");
    assert!(body.contains("\"world_seed\":7"), "status body: {body}");
    assert!(body.contains("\"uptime_secs\""), "status body: {body}");
    let health = http_get(port, "/healthz").await;
    assert_eq!(health, "ok");
}

/// Tiny HTTP/1.1 GET over a raw TcpStream (avoids pulling in an HTTP client
/// dependency just for two endpoints); returns the response body.
async fn http_get(port: u16, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("tcp connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .expect("send request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let text = String::from_utf8(response).expect("utf8 response");
    let (_, body) = text.split_once("\r\n\r\n").expect("response has a body");
    body.to_string()
}
