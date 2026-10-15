# claudo

A native Claude client, without Electron, that runs on your Claude subscription by driving
the Claude Code CLI.

## Run

You need Rust and the [Claude Code CLI](https://code.claude.com), logged in with your own
subscription:

```bash
claude auth login
```

### COSMIC

```bash
cargo cosmic-release
```

That is `cargo run -p claudo-cosmic --features cosmic --release`. Without
`--features cosmic` the binary only prints a message and exits.

### egui (Linux, macOS, Windows)

```bash
cargo egui
```

### Browser

Once:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126   # must match Cargo.lock
```

Then build the bundle and serve it:

```bash
./scripts/build-web.sh
cargo daemon
```

Open the URL the daemon prints; it carries the auth token. `CLAUDO_PORT` and
`CLAUDO_TOKEN` pin the port and token.

## Features

- Conversations in a sidebar, read from Claude Code's own transcripts, so they are shared with the CLI.
- Streaming replies you can stop, with Claude's thinking and tool calls shown as collapsible rows.
- Permission profiles: Safe, Accept edits, Full auto.
- Model and thinking level, globally or per conversation.
- Context usage for the last turn.
- A scratch working directory per conversation, or an attached project folder.
- Zoom with Ctrl+scroll or Ctrl+`+`/`-`/`0`.
- COSMIC: several agents side by side in tiles (below).

The COSMIC frontend cannot attach a project folder yet.

### Tiles (COSMIC)

| Do | How |
|---|---|
| Open a conversation in the focused tile | Click it |
| Open it in a new tile at the right | Shift+click, or right-click → Open in new tile |
| New conversation | New chat or Ctrl+N; Shift+click or Ctrl+Shift+N for a new tile |
| Resize tiles | Drag the line between two tiles |
| Move a tile | Drag its title, right-click it for Move left/right, or Ctrl+Shift+PageUp/PageDown |
| Focus the previous or next tile | Ctrl+PageUp/PageDown |
| Close a tile | Its ✕, or Ctrl+W |

A conversation is in at most one tile. Closing a tile does not stop its agent: the sidebar
shows pulsing dots beside each agent still working, and an Approve badge beside one waiting
for your answer.

## How it works

Every frontend talks to one backend (`crates/core`), which keeps a `claude -p` process per
conversation and turns its `stream-json` output into events. claudo never handles tokens:
the CLI owns login and the agent loop. Details are in [CLAUDE.md](CLAUDE.md) and
[PLAN.md](PLAN.md).

## Good to know

- Everyone logs in with their own subscription. Turns use the same limits as Claude Code.
- Opening an issue or a pull request (`gh` or GitHub's MCP server) always waits for your approval, in every permission profile. Everything else follows the profile.
- Each agent is named after its conversation's title, so your agents, and Claude Code sessions, can find each other with `ListAgents` and talk with `SendMessage`. A message one receives shows as "From <agent>" and runs a turn as if you had sent it. Only a conversation with a running `claude` process (used in the last 30 minutes) can receive one.
- claudo keeps its data in `~/.local/share/claudo/`. Message history stays in `~/.claude/projects/`.

## Development

```bash
cargo build --workspace     # everything except COSMIC
cargo lint                  # clippy on everything, COSMIC included
cargo test -p claudo-core    # stream-json parser tests, no CLI needed
cargo probe live_probe      # probes hit the real CLI and cost tokens
```

`cargo probe` alone lists the probes. The aliases live in `.cargo/config.toml`.

### Live reload (COSMIC)

```bash
cargo install cargo-hot --version 0.1.1 --locked   # once
cargo cosmic-hot
```

It needs the dev build. A release build loads each patch but keeps running the old
view code, which the optimizer inlined into callers the patch does not replace.

Saving a file in `crates/frontends/cosmic/src/` patches the running window in about two
seconds, keeping its state. Changing a type (a new `Message` variant or field) or another
crate still needs a restart. On Linux it needs a system `lld` (Fedora: `sudo dnf install lld`).

## License

MIT. The bundled font is BIZ UDPGothic, under the SIL Open Font License (`assets/fonts/OFL.txt`).
