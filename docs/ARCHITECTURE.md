# Ferraria Architecture

Read this before touching any crate. `docs/DESIGN.md` holds the game-design
numbers (tiles, items, recipes, enemy stats, boss AI); this file holds the
engineering rules.

## Crates

- `crates/shared` — everything both sides need: tile/wall/item/entity
  definitions and their static data tables, world model (`World`, `Tile`,
  chunk encoding), the wire protocol (`protocol.rs`), and pure game logic that
  must agree on both sides (player physics step, collision, crafting recipe
  checks). **No tokio, no macroquad, no I/O here.** Must compile for both
  native and wasm32.
- `crates/server` — axum HTTP + WebSocket. Owns the authoritative `World`,
  runs the simulation at 60 tps, persists to disk. Serves `web/` statically.
- `crates/client` — macroquad, ships only as wasm32-unknown-unknown. Renders,
  predicts own player, mirrors server state, draws UI.

## Authority model (mirrors Terraria's)

- **Own player movement: client-authoritative.** The client simulates its own
  player with the shared physics step and sends `PlayerState` ~20/s. The
  server sanity-clamps (speed/teleport limits) and rebroadcasts to others.
- **World, enemies, NPCs, bosses, dropped items, inventory, player HP:
  server-authoritative.** Clients send intents (`HitTile`, `PlaceTile`,
  `UseItem`, `Craft`, `MoveInventorySlot`, `TalkToNpc`, `BuyItem`); the server
  validates, mutates, and broadcasts deltas. The client may render optimistic
  effects but must reconcile to server state.
- Other players/entities are rendered with ~100 ms interpolation between
  snapshot positions.

## Wire protocol

- Binary WebSocket frames, `postcard`-encoded `ClientMessage` /
  `ServerMessage` enums in `shared/src/protocol.rs`.
- **Append new variants at the end of enums.** Renumbering breaks postcard
  compatibility; bump `PROTOCOL_VERSION` on any breaking change.
- Chunks: 64x64 tiles, lz4-compressed (`lz4_flex`), pushed by the server for
  the 3x3 chunk neighborhood around each player and on subscription change.
- Entity snapshots broadcast every 3 ticks (20/s). Tile changes broadcast
  immediately as deltas.

## Simulation

- Fixed 60 tps on the server; one single-threaded sim task owns the `World`
  (no locks in game logic). Network sessions talk to it via mpsc channels.
- All gameplay constants live in `shared` data tables — no magic numbers
  inline in sim code.
- Positions are `f32` in tile units (1 tile = 16 px on screen). Velocities in
  tiles/tick is forbidden — use tiles/second and multiply by `DT`.

## Persistence

- World + players serialize with postcard to `$DATA_DIR` (default `data/`,
  `/data` on Railway) every 60 s and on graceful shutdown. Players are keyed
  by name + a secret token the server issues on first join (stored in browser
  localStorage).

## Workflow (every feature)

1. Branch `feat/<name>` off latest `main`; work in your own git worktree.
2. `scripts/check.sh` must pass (fmt, clippy -D warnings native+wasm, tests,
   wasm build). All cargo commands go through `scripts/dev.sh` (host has no C
   toolchain).
3. Push branch, open a PR with `gh pr create`, await review, merge with
   `gh pr merge --squash`.
4. Add unit tests for pure logic (world gen invariants, crafting, physics,
   protocol roundtrips). Rendering/UI is exempt.

## Style

- No `unwrap()` outside tests/startup; in the sim loop, log and continue.
- Data-driven: new tiles/items/enemies = new rows in the static tables in
  `shared`, not new code paths, wherever possible.
