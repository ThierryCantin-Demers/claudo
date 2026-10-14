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
cargo run -p claudo-cosmic --features cosmic --release
```

`--features cosmic` is required. Without it the binary only prints a message and exits.

### egui (Linux, macOS, Windows)

```bash
cargo run -p claudo-egui --release
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
cargo run -p claudo-daemon
```

Open the URL the daemon prints; it carries the auth token. `CLAUDO_PORT` and
`CLAUDO_TOKEN` pin the port and token.

## Features

- Conversations in a sidebar, read from Claude Code's own transcripts, so they are shared with the CLI.
- Streaming replies, with tool calls shown as collapsible cards.
- Permission profiles: Safe, Accept edits, Full auto.
- Model and thinking level, globally or per conversation.
- Context usage for the last turn.
- A scratch working directory per conversation, or an attached project folder.
- Zoom with Ctrl+scroll or Ctrl+`+`/`-`/`0`.

The COSMIC frontend is behind egui: plain text instead of markdown, and tool output is not collapsible.

## How it works

Every frontend talks to one backend (`crates/core`), which keeps a `claude -p` process per
conversation and turns its `stream-json` output into events. claudo never handles tokens:
the CLI owns login and the agent loop. Details are in [CLAUDE.md](CLAUDE.md) and
[PLAN.md](PLAN.md).

## Good to know

- Everyone logs in with their own subscription. Turns use the same limits as Claude Code.
- There is no approve/deny prompt per tool call yet. Pick a permission profile instead.
- claudo keeps its data in `~/.local/share/claudo/`. Message history stays in `~/.claude/projects/`.

## Development

```bash
cargo build --workspace                        # everything except COSMIC
cargo clippy --workspace --examples
cargo run -p claudo-core --example live_probe  # probes hit the real CLI and cost tokens
```

The other probes are in `crates/core/examples/` and `crates/daemon/examples/`.

## License

MIT. The bundled font is BIZ UDPGothic, under the SIL Open Font License (`assets/fonts/OFL.txt`).
