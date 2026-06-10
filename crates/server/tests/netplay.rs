//! End-to-end netplay test: boots the real server (axum + sim task) on an
//! ephemeral port with a small generated world and drives it with real
//! WebSocket clients (tokio-tungstenite).

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use ferraria_shared::items::{inventory, InvSlot, STARTING_KIT};
use ferraria_shared::protocol::{decode, encode, ClientMessage, ServerMessage};
use ferraria_shared::{CHAT_MAX_CHARS, PROTOCOL_VERSION};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
/// Small but above the worldgen minimum (300×300); generates in well under
/// a second even in debug builds.
const TEST_WORLD: (u64, u32, u32) = (7, 300, 300);

/// Boots a server on an ephemeral port; returns the port.
async fn start_server() -> u16 {
    let (seed, w, h) = TEST_WORLD;
    let world = ferraria_server::worldgen::generate_with_size(seed, w, h);
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
    while chunks == 0 || inv.is_none() || !time {
        match recv(&mut a).await {
            ServerMessage::ChunkData { bytes, .. } => {
                ferraria_shared::world::decode_chunk(&bytes).expect("chunk decodes");
                chunks += 1;
            }
            ServerMessage::InventorySync { slots } => inv = Some(slots),
            ServerMessage::TimeSync { .. } => time = true,
            other => panic!("unexpected join frame {other:?}"),
        }
    }
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
