//! WebSocket facade over the `web/quad_ws.js` miniquad plugin
//! (docs/NETWORKING.md). Binary frames carry postcard-encoded protocol
//! messages; everything is polled from the frame loop — no threads on wasm.
//!
//! The client ships wasm-only. The native build keeps a stub that fails to
//! connect and reports a permanently closed socket, so the crate still
//! compiles for tooling on the host target.

use ferraria_shared::protocol::{self, ClientMessage, ServerMessage};

// FFI contract implemented by `web/quad_ws.js` (docs/NETWORKING.md):
// status is 0 = connecting / 1 = open / 2 = closed-or-error; next_len is the
// next pending message length (or -1 if none); recv copies the next message
// into (ptr, cap) returning bytes written (or -1); default_url writes the
// location-derived ws URL into (ptr, cap) returning bytes written (or -1).
#[cfg(target_arch = "wasm32")]
extern "C" {
    fn quad_ws_connect(url_ptr: *const u8, url_len: usize);
    fn quad_ws_status() -> i32;
    fn quad_ws_send(ptr: *const u8, len: usize);
    fn quad_ws_next_len() -> i32;
    fn quad_ws_recv(ptr: *mut u8, cap: usize) -> i32;
    fn quad_ws_default_url(ptr: *mut u8, cap: usize) -> i32;
}

/// Version handshake with the JS plugin: the miniquad loader compares this
/// export against the `version` that `quad_ws.js` registers with and logs an
/// error on mismatch. Bump both sides together.
#[no_mangle]
pub extern "C" fn quad_ws_crate_version() -> u32 {
    1
}

/// Connection state as reported by the JS side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsStatus {
    Connecting,
    Open,
    /// Closed or errored; terminal — reconnect by building a new [`WsClient`].
    Closed,
}

/// One WebSocket connection to the game server.
pub struct WsClient {
    /// Frames that arrived but didn't decode as a [`ServerMessage`]
    /// (protocol bug or corruption); surfaced in the debug overlay.
    pub bad_frames: u32,
}

impl WsClient {
    /// Opens a connection to `/ws` on the host serving the page (scheme
    /// `wss` on https pages). Errors if the URL can't be derived — on wasm
    /// that means `quad_ws.js` isn't loaded (the bundle stubs missing
    /// imports with no-ops, see docs/NETWORKING.md gotchas).
    pub fn connect_to_page_server() -> Result<WsClient, String> {
        #[cfg(target_arch = "wasm32")]
        {
            let mut buf = [0u8; 256];
            let n = unsafe { quad_ws_default_url(buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                return Err(
                    "could not derive the server URL from the page (quad_ws.js missing?)".into(),
                );
            }
            let url = std::str::from_utf8(&buf[..n as usize])
                .map_err(|_| "server URL is not valid UTF-8".to_string())?;
            unsafe { quad_ws_connect(url.as_ptr(), url.len()) };
            Ok(WsClient { bad_frames: 0 })
        }
        #[cfg(not(target_arch = "wasm32"))]
        Err("the Ferraria client is wasm-only; run it in a browser (scripts/build-web.sh)".into())
    }

    pub fn status(&self) -> WsStatus {
        #[cfg(target_arch = "wasm32")]
        {
            match unsafe { quad_ws_status() } {
                0 => WsStatus::Connecting,
                1 => WsStatus::Open,
                _ => WsStatus::Closed,
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        WsStatus::Closed
    }

    pub fn is_open(&self) -> bool {
        self.status() == WsStatus::Open
    }

    pub fn is_closed(&self) -> bool {
        self.status() == WsStatus::Closed
    }

    /// Encodes and sends one message. Silently dropped unless open (the
    /// caller drives reconnects off [`WsClient::status`], not send errors).
    pub fn send(&self, msg: &ClientMessage) {
        if !self.is_open() {
            return;
        }
        let bytes = protocol::encode(msg);
        #[cfg(target_arch = "wasm32")]
        unsafe {
            quad_ws_send(bytes.as_ptr(), bytes.len())
        };
        #[cfg(not(target_arch = "wasm32"))]
        drop(bytes);
    }

    /// Drains every message received since the last call — call once per
    /// frame. Undecodable frames are counted in `bad_frames` and skipped.
    pub fn drain(&mut self) -> Vec<ServerMessage> {
        #[cfg(target_arch = "wasm32")]
        {
            let mut out = Vec::new();
            loop {
                let len = unsafe { quad_ws_next_len() };
                if len < 0 {
                    break;
                }
                let mut buf = vec![0u8; len as usize];
                let got = unsafe { quad_ws_recv(buf.as_mut_ptr(), buf.len()) };
                if got < 0 {
                    break;
                }
                buf.truncate(got as usize);
                match protocol::decode::<ServerMessage>(&buf) {
                    Some(msg) => out.push(msg),
                    None => self.bad_frames = self.bad_frames.wrapping_add(1),
                }
            }
            out
        }
        // The native stub never receives (`mut out` here would trip the
        // native client clippy lane's unused_mut).
        #[cfg(not(target_arch = "wasm32"))]
        Vec::new()
    }
}
