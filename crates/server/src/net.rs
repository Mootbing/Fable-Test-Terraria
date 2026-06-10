//! HTTP + WebSocket front end: the axum router (`/ws`, `/healthz`,
//! `/api/status`, static `web/`) and the per-connection session actor.
//!
//! A session is a thin byte pump (ARCHITECTURE.md "Simulation"): it performs
//! the version half of the handshake, registers with the sim task, then
//! forwards decoded [`ClientMessage`]s inward over the shared command mpsc
//! and encoded frames outward from its per-session queue. All game logic and
//! all other validation live in [`crate::sim::game`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tower_http::services::ServeDir;

use ferraria_shared::protocol::{decode, encode, ClientMessage, ServerMessage};
use ferraria_shared::PROTOCOL_VERSION;

use crate::sim::game::{self, Frame, Sim, SimCommand, COMMAND_QUEUE, OUTBOUND_QUEUE_FRAMES};

/// A client must send `Hello` within this long of connecting.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct AppState {
    sim_tx: mpsc::Sender<SimCommand>,
    seed: u64,
    started: Instant,
    player_count: Arc<AtomicUsize>,
}

/// Builds the full server router and spawns the sim task for `world`.
/// Must be called from within a tokio runtime.
pub fn router(world: ferraria_shared::world::World, seed: u64, web_dir: &str) -> Router {
    let player_count = Arc::new(AtomicUsize::new(0));
    let (sim_tx, sim_rx) = mpsc::channel(COMMAND_QUEUE);
    let sim = Sim::new(world, player_count.clone());
    tokio::spawn(game::run(sim, sim_rx));
    let state = AppState {
        sim_tx,
        seed,
        started: Instant::now(),
        player_count,
    };
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/status", get(status))
        .route("/ws", get(ws_upgrade))
        .fallback_service(ServeDir::new(web_dir))
        .with_state(state)
}

/// Minimal liveness JSON for Railway monitoring.
#[derive(serde::Serialize)]
struct Status {
    players: usize,
    world_seed: u64,
    uptime_secs: u64,
}

async fn status(State(state): State<AppState>) -> Json<Status> {
    Json(Status {
        players: state.player_count.load(Ordering::Relaxed),
        world_seed: state.seed,
        uptime_secs: state.started.elapsed().as_secs(),
    })
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| session(socket, state.sim_tx.clone()))
}

/// One task per connection: handshake, then pump until either side closes.
async fn session(mut socket: WebSocket, sim_tx: mpsc::Sender<SimCommand>) {
    let Some((player_id, outbound_rx)) = handshake(&mut socket, &sim_tx).await else {
        return;
    };
    pump(socket, &sim_tx, player_id, outbound_rx).await;
    // Idempotent: the sim ignores ids it already removed (kicks).
    let _ = sim_tx.send(SimCommand::Disconnect { player_id }).await;
}

/// Reads `Hello`, gates `PROTOCOL_VERSION` here, and delegates the stateful
/// checks (full server, duplicate name, token reclaim) to the sim. Returns
/// the assigned player id and the queue the sim will write frames into;
/// `None` means the connection was rejected/closed (any `Reject` has been
/// sent).
async fn handshake(
    socket: &mut WebSocket,
    sim_tx: &mpsc::Sender<SimCommand>,
) -> Option<(u32, mpsc::Receiver<Frame>)> {
    let hello = loop {
        // Timeout, closed socket, or transport error: just drop the
        // connection — there is no one to talk to.
        let Ok(Some(Ok(frame))) = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.recv()).await
        else {
            return None;
        };
        match frame {
            Message::Binary(bytes) => break decode::<ClientMessage>(&bytes),
            Message::Close(_) => return None,
            // tungstenite answers pings itself; anything else pre-Hello is
            // noise.
            _ => continue,
        }
    };
    let Some(ClientMessage::Hello {
        protocol_version,
        name,
        token,
    }) = hello
    else {
        reject(socket, "expected a Hello frame".into()).await;
        return None;
    };
    if protocol_version != PROTOCOL_VERSION {
        reject(
            socket,
            format!(
                "protocol version mismatch: client {protocol_version}, server {PROTOCOL_VERSION} — refresh the page"
            ),
        )
        .await;
        return None;
    }
    let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_FRAMES);
    let (reply_tx, reply_rx) = oneshot::channel();
    sim_tx
        .send(SimCommand::Join {
            name,
            token,
            tx,
            reply: reply_tx,
        })
        .await
        .ok()?;
    match reply_rx.await {
        Ok(Ok(player_id)) => Some((player_id, rx)),
        Ok(Err(reason)) => {
            reject(socket, reason).await;
            None
        }
        Err(_) => None, // sim shutting down
    }
}

async fn reject(socket: &mut WebSocket, reason: String) {
    tracing::info!(%reason, "rejecting connection");
    let frame = encode(&ServerMessage::Reject { reason });
    let _ = socket.send(Message::Binary(frame.into())).await;
    let _ = socket.send(Message::Close(None)).await;
}

/// Forwards sim frames out and client frames in until the socket dies or
/// the sim drops the outbound queue (kick).
async fn pump(
    socket: WebSocket,
    sim_tx: &mpsc::Sender<SimCommand>,
    player_id: u32,
    mut outbound_rx: mpsc::Receiver<Frame>,
) {
    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            let bytes = axum::body::Bytes::copy_from_slice(&frame);
            if sink.send(Message::Binary(bytes)).await.is_err() {
                return;
            }
        }
        // The sim dropped us (kick): close politely so the read half ends.
        let _ = sink.send(Message::Close(None)).await;
    });

    while let Some(Ok(frame)) = stream.next().await {
        match frame {
            Message::Binary(bytes) => match decode::<ClientMessage>(&bytes) {
                Some(msg) => {
                    if sim_tx
                        .send(SimCommand::Message { player_id, msg })
                        .await
                        .is_err()
                    {
                        break; // sim gone; shut the session down
                    }
                }
                None => {
                    tracing::debug!(player = player_id, "ignoring undecodable frame");
                }
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
    writer.abort();
}
