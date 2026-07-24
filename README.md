# claudo

A native, non-Electron GUI for Claude that drives the authenticated **Claude Code CLI** underneath.

Because the CLI owns authentication and the agent loop, claudo runs on your **Claude subscription** without ever handling tokens or implementing an OAuth flow. It is a frontend over `claude -p`.

Three frontends share one backend:

| Frontend | Target | Status |
|---|---|---|
| **egui** (`claudo`) | native desktop (Linux/macOS/Windows) | primary, full featured |
| **egui / wasm** | web client served by the daemon | full featured |
| **libcosmic** (`claudo-cosmic`) | native COSMIC desktop | opt-in, reduced feature set |

## Requirements

- Rust (stable, 2021 edition)
- [Claude Code CLI](https://code.claude.com) on `PATH`, logged in with your own subscription:
  ```bash
  claude auth login
  ```
  claudo checks this at startup via `claude auth status --json` and refuses to run without it.

## Quick start

Native app:

```bash
cargo run -p claudo-egui --release
```

Web client (build the bundle, then serve it):

```bash
./scripts/build-web.sh
cargo run -p claudo-daemon
```

The daemon prints a URL containing a per run auth token, for example
`http://127.0.0.1:8787/?token=<token>`. Open that.

The web build additionally needs:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126   # must match Cargo.lock
```

COSMIC frontend (opt-in, pulls a large dependency tree):

```bash
cargo run -p claudo-cosmic --features cosmic --release
```

Without `--features cosmic` that binary is a stub that prints instructions. A plain
`cargo build --workspace` never compiles libcosmic.

## Features

- **Conversations** with a sidebar, backed by Claude Code's own transcript store. History is reconstructed from `~/.claude/projects/**/*.jsonl`, so conversations survive restarts and are shared with the CLI.
- **Streaming** with a smooth reveal. The CLI emits text in large chunks, so words fade in at the leading edge instead of appearing in jumps.
- **Tool activity** rendered as collapsible cards with status (running, done, denied).
- **Permission profiles**: Safe (tools needing approval are denied), Accept edits, Full auto. Maps to `--permission-mode`.
- **Model and thinking level**, including specific versions (`claude-opus-4-7`) and the CLI aliases. Defaults to `claude-opus-5`.
- **Global settings plus per conversation overrides.** Each override falls back to the global value when unset.
- **Context usage** for the last turn, for example `12.2k / 1.0M ctx (1%)`, read from the CLI's own reporting.
- **Per conversation working directory**: a dedicated scratch dir by default, or attach an existing project folder.
- **Zoom**: Ctrl+scroll and Ctrl+`+`/`-`/`0`.
- **Japanese text** via a bundled font (BIZ UDPGothic, SIL OFL), embedded in the binary so it works identically on every machine and in the browser.

## Architecture

Two deliberate boundaries:

```
  egui (native)   egui (wasm)   libcosmic
        \             |            /
         \            |           /          Command / ServerEvent
          +-----------+----------+           (crates/protocol)
                      |
              in-process channel  or  WebSocket
                      |
                   Backend                   (crates/core)
                      |
              claude -p (subprocess)
```

1. **UI to backend is a versioned protocol.** Frontends are dumb: they render `ServerEvent`s and emit `Command`s. All state and logic lives in `core`. Adding a frontend touches no backend code.
2. **Backend to Claude is the CLI process boundary.** `core` never speaks HTTP to Anthropic. It spawns `claude` and translates its `stream-json` output into protocol events.

Crates:

| Crate | Role |
|---|---|
| `protocol` | shared serde types: `Command`, `ServerEvent`, `Item`, `Settings`, `Overrides` |
| `core` | backend: engine (CLI driver), conversation store, transcript reader, settings |
| `transport` | `Transport` trait, in-process channel (native), WebSocket client (wasm) |
| `daemon` | WebSocket server on `127.0.0.1` plus static hosting of the web bundle |
| `frontends/egui` | default frontend, compiles to native and wasm |
| `frontends/cosmic` | libcosmic frontend, behind the `cosmic` feature |

One turn runs as:

```bash
claude -p "<prompt>" \
  --output-format stream-json --include-partial-messages --verbose \
  --permission-mode <default|acceptEdits|bypassPermissions> \
  [--resume <session-id>] [--model <model>] [--effort <level>]
```

Turn one creates a session, later turns resume it. A subprocess exists only while a turn is streaming; idle conversations are just files on disk.

## Data locations

| Path | Contents |
|---|---|
| `~/.local/share/claudo/conversations.json` | conversation index (title, cwd, session id, overrides) |
| `~/.local/share/claudo/settings.json` | global settings |
| `~/.local/share/claudo/conversations/<id>/` | per conversation scratch working directory |
| `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl` | message history, owned by Claude Code |

Environment overrides: `CLAUDO_CLAUDE_BIN`, `XDG_DATA_HOME`, and for the daemon
`CLAUDO_PORT`, `CLAUDO_TOKEN`, `CLAUDO_STATIC_DIR`.

## Usage scope

This wraps Claude Code's documented headless mode. It never replays an OAuth flow and stores no tokens.

- Personal use, or a small internal share. Not a published product.
- **Every user logs in with their own Claude subscription** through their own CLI. Never point it at a shared login on someone else's behalf.
- Human paced chat, not high volume automation. Turns draw on the same rate limits as interactive Claude Code.
- It depends on CLI headless behaviour that Anthropic could change. There is no API key fallback by design.

## Limitations

- **No true per call permission prompts.** The CLI does not emit a permission request in headless mode; it auto denies. Live approve/deny would require replicating the Agent SDK's undocumented control protocol. Permission profiles are the supported lever instead.
- **libcosmic frontend is behind the others**: plain text (no markdown), no settings dialog beyond the inline dropdowns, and zoom scales text only, because libcosmic exposes no runtime window scale factor.
- The web bundle is roughly 14 MB, mostly the embedded Japanese font. Fine over localhost; subsetting the font would shrink it substantially.

## Development

```bash
cargo build --workspace              # everything except the cosmic frontend
cargo clippy --workspace --examples
```

Manual probes that exercise the backend against the real CLI:

```bash
cargo run -p claudo-core --example turn             # one turn, tool permissions
cargo run -p claudo-core --example sessions         # transcript reload
cargo run -p claudo-core --example backend_probe    # protocol round trip
cargo run -p claudo-core --example settings_probe   # settings persistence
cargo run -p claudo-core --example overrides_probe  # per conversation overrides
cargo run -p claudo-daemon --example ws_probe       # WebSocket round trip
```

Dependencies are compiled with `opt-level = 3` even in dev builds, because
unoptimized text shaping makes the cosmic frontend unusably slow.

See [PLAN.md](PLAN.md) for the design rationale and milestone history.

## License

MIT. The bundled font is BIZ UDPGothic, SIL Open Font License, see `assets/fonts/OFL.txt`.
