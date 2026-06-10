# Client networking (wasm32 + macroquad)

Verified research, June 2026. Do not deviate without re-verifying.

## Decision

WebSockets via **our own miniquad JS plugin** (`web/quad_ws.js`, already in repo)
with raw pointer+length FFI. **No `quad-net`** — its crates.io release cannot
receive binary frames on wasm and the plugin embedded in the hosted
`mq_js_bundle.js` throws `ReferenceError` on first use. Native (non-wasm)
client builds may use `ewebsock` behind the same facade, gated with
`[target.'cfg(not(target_arch = "wasm32"))'.dependencies]` (its web backend is
wasm-bindgen and must never be enabled on wasm).

`web/mq_js_bundle.js` is vendored from the macroquad repo (gl.js protocol
version 2, matches macroquad 0.4.15 / miniquad 0.4.10). Keep it in lockstep if
macroquad is bumped. Script order in index.html: bundle → quad_ws.js → `load()`.

## FFI contract (implemented by web/quad_ws.js)

```rust
extern "C" {
    fn quad_ws_connect(url_ptr: *const u8, url_len: usize);
    fn quad_ws_status() -> i32;            // 0 connecting, 1 open, 2 closed/error
    fn quad_ws_send(ptr: *const u8, len: usize);
    fn quad_ws_next_len() -> i32;          // next msg len, -1 if none
    fn quad_ws_recv(ptr: *mut u8, cap: usize) -> i32;
    fn quad_ws_default_url(ptr: *mut u8, cap: usize) -> i32; // page-derived
        // ws URL ("ws(s)://<host>/ws") copied into (ptr, cap); returns bytes
        // written, -1 if it doesn't fit. Synchronous copy, no retained views.
}
#[no_mangle] pub extern "C" fn quad_ws_crate_version() -> u32 { 1 }
```

Wrap in a `WsClient` facade (`connect`, `is_open`, `is_closed`, `send(&[u8])`,
`try_recv() -> Result<Option<Vec<u8>>, String>`); drain all messages every
frame in the game loop. Binary frames only.

The ws URL is derived in JS from `location` (ws:// vs wss:// by page scheme,
same host, path `/ws`) or hardcoded relative; https pages require wss.

## Gotchas (each one verified)

- A missing `<script src="quad_ws.js">` does NOT fail the build or load — gl.js
  stubs unresolved imports with console.warn no-ops. If networking silently
  does nothing, check the browser console first.
- No threads on wasm: all recv/send happens in the frame loop.
- `rand`/`getrandom` crates don't work on wasm32-unknown-unknown here — use
  `macroquad::rand` (quad-rand) client-side; shared/server code needs its own
  seedable PRNG (no getrandom).
- High-DPI: set `Conf { high_dpi: true, .. }`; use screen_width()/height().
- Pixel art: `texture.set_filter(FilterMode::Nearest)`.
- Audio: browsers block until first user gesture (bundle auto-resumes on first
  click); gate music behind a click-to-start screen.
- Edition 2021 in this repo: plain `extern "C"` blocks are fine.
- `.cargo/config.toml` passes `--import-undefined` to the wasm linker — that is
  what lets the JS-provided imports link (required on rustc >= 1.87).
