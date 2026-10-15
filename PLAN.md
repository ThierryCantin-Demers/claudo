# claudo: Native Claude Client (Claude Code CLI frontend): Implementation Plan

Status: M0 to M5 and M3b implemented; M6 (polish) in progress. COSMIC is the primary frontend, so its gaps come first.

## Decisions locked (from review)

- **Directory:** `/home/thierrycd/prog/claude-native-client/` (for now).
- **First frontend:** egui (also unlocks the web client). libcosmic later.
- **Permission posture:** **prompt-per-tool-call by default**, with a toggle to **auto mode** (pre-approve tools) per conversation / globally.
- **No API-key fallback.** Subscription-via-CLI is the only engine. Keeps the project single-purpose.
- **Distribution:** a **private shared GitHub repo**; coworkers build from source and each logs in with **their own** Claude subscription. Not published publicly.
- **Conversation cwd:** **dedicated per-conversation data dir by default**, with an optional "attach to a project folder" mode.
- **Subprocess model:** **one long-lived process per conversation** (bidirectional stream-json), closed after 30 minutes idle and respawned with `--resume`. Started as spawn-per-turn.
- **Name:** **`claudo`** (for now). Used as the workspace/binary name; directory stays `claude-native-client/` for now.

All review questions resolved.

> **Architecture in one line:** a native, non-Electron GUI that drives the **authenticated Claude Code CLI** underneath (`claude -p --output-format stream-json`). The CLI owns auth and the agent loop; this app is a nice frontend over it. That's how it uses the Claude **subscription** without putting any auth/OAuth in the app itself.

## 0. Usage scope and terms constraint (read first)

This tool wraps the **Claude Code CLI in headless mode**: a documented, scriptable feature. It never handles tokens directly and never replays the OAuth flow. Auth is entirely Claude Code's (`claude setup-token` / `claude` login).

Hard constraints to stay on the defensible side of Anthropic's terms:

- **Personal use, or a small internal share with coworkers via a private GitHub repo: not a published product.** Coworkers clone and build from source.
- **Every user logs in with their OWN Claude subscription** via their own CLI. The app is only a UI; it never routes anyone's requests through someone else's credentials.
- **Never** ship or point it at a single shared subscription login on behalf of others: that's the explicitly-banned "route requests through Free/Pro/Max credentials on behalf of their users" pattern.
- **Human-paced chat, not API-scale automation.** Don't turn it into a batch/throughput tool.
- Accept that this relies on CLI headless behavior that Anthropic could change (e.g. lock headless to `--bare`, which drops subscription auth). If that happens, the subscription path breaks and only API-key mode survives.

Prior art this mirrors (documented, working): [sugyan/claude-code-webui](https://github.com/sugyan/claude-code-webui), [BSWEN stream-json UI guide](https://docs.bswen.com/blog/2026-03-21-stream-json-custom-ui-claude-code/), [avasdream: wrapping the CLI](https://avasdream.com/blog/claude-cli-agentic-wrapper).

## 1. Goals

- A Claude client that is **not Electron** (no bundled Chromium), native GPU-rendered.
- Uses the **Claude subscription**: via the CLI, not an API key.
- **Cross-platform**: Linux, Windows, macOS.
- **Web client** support (secondary priority, designed for from day one).
- **Multi-frontend**: a default cross-platform GUI plus an optional COSMIC-native GUI.
- **Multiple conversations** with a Claude-Desktop-style sidebar, sourced from Claude Code's existing session store.

## 2. Non-goals (at least initially)

- Talking to the Anthropic API directly / API-key billing as the primary path (kept only as an optional fallback engine: see §9).
- Reimplementing the agent loop or tools: Claude Code already provides them.
- Full parity with official Claude Desktop (Cowork, voice, official Computer Use).
- Mobile.
- A published, multi-tenant, or hosted SaaS product (see §0).

## 3. Core design principle

Two boundaries, both deliberate:

1. **UI ↔ backend = a versioned protocol** (serde types over a WebSocket, or an in-process channel carrying the same types). Every frontend is "dumb": render state, emit commands. All logic lives in `core`. Frontends are swappable peers; adding libcosmic touches zero backend code.
2. **backend ↔ Claude = the CLI process boundary.** `core` never speaks HTTP to Anthropic in subscription mode; it spawns and manages `claude` subprocesses and translates their `stream-json` events into the protocol. Swapping the engine (CLI ↔ direct API) is confined to one module behind a trait.

## 4. Tech decisions (with rationale)

| Decision | Choice | Why |
|---|---|---|
| Language | Rust | Native, cross-platform, no runtime, good async + wasm story. |
| Default GUI | **egui** via `eframe` | Same code compiles to native (Win/Mac/Linux) **and** wasm/web. First-class, well-trodden. Serves the web-client goal directly. |
| Markdown render (egui) | `egui_commonmark` | Markdown + syntax-highlighted code blocks with little effort. |
| Optional native GUI | **libcosmic** (on iced) | Native COSMIC theming + real native widgets on the daily driver. Added later; off the critical path. |
| **Claude engine** | **Claude Code CLI** in `-p --output-format stream-json --input-format stream-json` mode | Subscription auth handled by the CLI; agent loop + tools + MCP come for free; documented, scriptable interface. |
| Subprocess mgmt | `tokio::process` (plain pipes) or a PTY lib if needed | Spawn/manage `claude`, stream stdin/stdout. Start with plain pipes; PTY only if the CLI needs a tty. |
| Conversation store | **Read Claude Code's own `~/.claude/projects/<encoded-path>/<session-id>.jsonl`** | No separate DB needed for history: the CLI already persists every conversation. App reads these for the sidebar + history. |
| Async runtime | `tokio` (native); wasm uses browser event loop | Standard. |
| Serialization | `serde` + `serde_json` | Protocol types + parsing the CLI's `stream-json`. |
| Transport | WebSocket (`tokio-tungstenite` server; `ewebsock`/gloo on wasm client) | One transport works native and in-browser. |

Rejected: Electron (the whole point), Tauri (still a webview), GTK4/relm4 (heavier, worse fit for streaming chat), Agent SDK as engine (requires API key: can't use subscription), direct `/v1/messages` in any form (would force API-key billing: dropped entirely per review).

## 5. Workspace layout

```
claude-native-client/       # dir name; workspace + binary are named `claudo`
  Cargo.toml              # workspace root
  crates/
    protocol/             # serde types: client->server commands, server->client events
    core/                 # backend: engine, CLI session manager, transcript reader, state
      engine/
        cli.rs            # spawn+drive `claude`, parse stream-json -> protocol events
                          # (kept behind a trait for isolation, but CLI is the only impl)
      sessions.rs         # read ~/.claude/projects/*.jsonl for the conversation list/history
    transport/            # WS server + in-proc channel adapter (both speak `protocol`)
    frontends/
      egui/               # default: native desktop + wasm web client (eframe)
      cosmic/             # libcosmic native frontend (documented stub initially)
    daemon/               # standalone backend binary (for web-client / remote mode)
  README.md
```

Dependency direction: `frontends/* -> protocol` (+ transport client side); `daemon -> core -> protocol`; `transport -> protocol`. Frontends never depend on `core`.

## 6. Protocol sketch (to be refined)

Client → Server (commands):
- `SendMessage { conversation_id, text, attachments? }`
- `NewConversation { cwd, system_prompt_append?, model? }`  (cwd = working dir for the CLI session: see §8)
- `CancelStream { conversation_id }`  → sends interrupt / kills the subprocess
- `ApproveToolCall { call_id, approved: bool }`  (relays Claude Code's own permission prompt to the UI)
- `ListConversations`, `LoadConversation { session_id }`

Server → Client (events, mapped from the CLI's `stream-json`):
- `StreamDelta { conversation_id, text }`  ← assistant text deltas
- `ThinkingDelta { conversation_id, text }`  ← optional, if surfaced
- `ToolCallStarted { conversation_id, call_id, tool, args }`
- `ToolCallResult { conversation_id, call_id, result }`
- `PermissionRequested { conversation_id, call_id, tool, args }`  ← Claude Code awaiting approval
- `MessageComplete { conversation_id, message, session_id }`
- `Error { detail }`
- `State { conversations, current }`

Notes:
- Protocol is versioned (handshake) so web client and backend evolve independently.
- Stream reassembly happens in `core`, not the UI. The UI appends deltas and swaps in `MessageComplete`.
- The event set is shaped by Claude Code's actual `stream-json` schema: pin that schema before finalizing (see §12 unknowns).

## 7. Multiple-conversation model (the "like Claude Desktop" answer)

This maps almost 1:1 onto Claude Code's existing session store, so it scales cleanly:

- **Sidebar / conversation list = read files, no processes.** Enumerate `~/.claude/projects/<encoded-path>/*.jsonl`; each file is one conversation (title/summary, timestamps, message count, git branch). Scales to hundreds of conversations: pure file I/O.
- **Open a conversation = parse its `.jsonl`** to render history instantly. Still no process.
- **A subprocess is spawned on the first message sent**, via `claude -p --input-format stream-json --output-format stream-json [--resume <session-id>] ...`, and kept for later turns, which are written to its stdin. After 30 minutes idle it is closed and the conversation is just files on disk again.
- **Concurrency** = one live subprocess per conversation used in the last 30 minutes. Many displayed, few alive. Running several at once is fine (bounded by RAM).
- Lifecycle: spawn on send → stream events → on `result` the process waits for the next turn. A different profile, model or effort, or a message after the idle close, respawns it with `--resume`. Use **`--resume <explicit-session-id>`**, not `--continue`, for reliable threading (`--continue -p` can spawn a new session).

## 8. Design wrinkles specific to wrapping the CLI

- **Sessions are scoped to a working directory.** Claude Code is project-oriented; a session belongs to a cwd. For a general chat app, pick a cwd per conversation: e.g. a dedicated `~/.local/share/<app>/conversations/<id>/` dir, or the repo the chat is about. `NewConversation { cwd }` makes this explicit.
- **It's agent-shaped, not raw chat.** Claude Code's system prompt + tool loop are present. Steer with `--append-system-prompt` and tool allowlists; accept you're rendering an agent, not bare completions. Fine (arguably good) for this use case.
- **Permission prompts** normally block an interactive TTY. In headless mode use `--permission-mode` / `--allowedTools` to pre-authorize, OR surface `PermissionRequested` in the UI and feed the decision back. Decide the default posture in §11.
- **Coupling to CLI flags/output.** You're bound to Claude Code's CLI surface and `stream-json` schema, which can shift between versions. Isolate all of it in `engine/cli.rs` and pin a tested CLI version range.
- **Startup latency / process overhead**: paid once per process, not per turn, since processes are kept per conversation.

## 9. Auth / engine

- **Subscription-via-CLI is the only engine.** User authenticates once via `claude` login or `claude setup-token` (a long-lived OAuth token for scripts, tied to a subscription). The app stores **no** tokens and does **no** OAuth.
- On startup, detect that `claude` is installed and logged in; otherwise guide the user to run login. Surface a clear error if the CLI is missing or unauthenticated (this is the app's single hard dependency).
- **No API-key fallback** (decided in review). The engine stays behind a trait purely for isolation/testability, but CLI is the only implementation. If Anthropic ever breaks headless subscription auth, the project has no escape hatch by design: see §14.

## 10. Deployment shapes (all from one backend)

1. **Bundled desktop app**: egui native window + `core` in-process (in-memory channel, no socket). Single binary. Default UX.
2. **Standalone daemon**: `daemon` runs `core` + WS server bound to `127.0.0.1` with an auth token; drives the CLI on that machine.
3. **Web client**: egui → wasm, static page, connects to the local daemon over WebSocket. Browser tab, CLI-powered backend behind it.
4. **(Later) COSMIC app**: libcosmic frontend, same in-proc or WS connection.

Frontend selection at launch: build feature or runtime check (`XDG_CURRENT_DESKTOP` contains `COSMIC`) → cosmic, else egui.

## 11. Security

Even though tools live in Claude Code, exposing them through a socket is still an RCE surface.

- Bind the daemon to `127.0.0.1` only; never `0.0.0.0` without explicit, deliberate opt-in.
- Require a **per-session auth token** on the WS handshake (daemon-generated, handed to the client out-of-band).
- **Relay Claude Code's permission model to the UI.** Default = **prompt-per-tool-call**. A per-conversation (and global) **auto mode** toggle pre-approves tools for smoother unattended runs. Auto mode is opt-in and clearly indicated in the UI so it can't be left on unknowingly.
- Separate **read-only vs full-access** tool profiles via `--allowedTools`.
- wasm web client: it only ever talks to the daemon; it never sees credentials or spawns processes.
- Coworker share: each install uses that person's own login; no shared credentials pass through the app.

## 12. Milestones

**M0: Skeleton**
- Cargo workspace: `protocol`, `core` (+ `engine/`, `sessions.rs`), `transport`, `frontends/egui`, `daemon`, `frontends/cosmic` (stub).
- Detect `claude` on PATH and report login status. Empty egui window opens natively.

**M1: Streaming chat via the CLI (native, in-proc)** ← the "it actually works" milestone
- `engine/cli.rs`: spawn `claude -p --output-format stream-json --input-format stream-json`, feed a user turn, parse the JSONL event stream, emit `StreamDelta`/`MessageComplete`.
- egui native window: input box, send, render assistant markdown as it streams.
- Single conversation, capture the returned `session_id`.

**M2: Multiple conversations from the session store (done)**
- `sessions.rs`: claudo-owned index (`~/.local/share/claudo/conversations.json`) of {id, title, cwd, session_id, timestamps}; history read from Claude Code's transcripts, located by `<session-id>.jsonl` filename (no need to reverse the dir encoding). Titles come from the transcript's `ai-title`, falling back to the first prompt.
- Per-conversation cwd: dedicated `~/.local/share/claudo/conversations/<id>/` scratch dir by default, plus "📁 Folder…" (rfd picker) to attach a project folder.
- Left sidebar (new/attach + list sorted by recency), top header showing title + cwd, history reload on switch. Resume via `--resume` unchanged from M1.
- Verified end-to-end: create → turn → transcript reload reconstructs history + ai-title; store round-trips.

**M3: Tool calls + permissions** (split during implementation)
- **M3a (done):** surface `ToolStarted`/`ToolResult` as collapsible tool cards; per-conversation **permission profile** selector: Safe (`--permission-mode default`), Accept edits (`acceptEdits`), Full auto (`bypassPermissions`). Validated: Safe denies writes, Full auto executes.
- **M3b (done, scoped):** approve/deny for opening issues and pull requests, in every profile, through the CLI's `can_use_tool` control requests (`--permission-prompt-tool stdio` plus ask rules). Other calls keep following the profile.

**M4: Protocol boundary, then WebSocket + web client** (split during implementation)
- **M4a (done):** real protocol (`Command`/`ServerEvent` + shared `Item`/`ToolCall`/`ConversationInfo`/`Profile` in the protocol crate); a transport-agnostic `Backend` in core (owns the store, drives the engine, emits events over a tokio unbounded channel); an `InProc` transport (backend on a worker thread, in-memory channels); egui frontend refactored to send commands + render events with **no direct `core` dependency** (it depends on `transport` + `protocol`). Verified via `backend_probe` example.
- **M4b (done):** `daemon` is an axum WS server on `127.0.0.1` (random token, or `CLAUDO_TOKEN`; `?token=` checked on the WS upgrade) that wraps a per-connection `Backend` and serves the web bundle via `ServeDir`. The egui frontend compiles to wasm with an `ewebsock` `WsClient` transport (reads host + `?token=` from the page URL); `Transport::send` is now `&mut self`; `rfd`/folder-picker gated to native. `cargo xtask web` builds the bundle (wasm-bindgen) into `crates/frontends/egui/dist` (gitignored; source `index.html` in `crates/frontends/egui/web/`). Verified: Rust WS client runs a full turn over the socket, bad token → 401, and the daemon serves `index.html`/`claudo.js`/`claudo_bg.wasm` with correct MIME types. Live in-browser render not checked here (sandbox blocks localhost in the browser pane).

**M5: COSMIC native frontend (done)**
- `frontends/cosmic` implemented against the same `transport` + `protocol`: a libcosmic `Application` (sidebar + conversation list, streaming transcript, tool lines, composer) driven by a ~30 Hz polling subscription over `Transport::poll`. Reads global `Settings` (no per-conversation override UI yet); plain-text rendering (markdown TBD).
- **Opt-in**: libcosmic (git, pinned rev `7e77707`) is an optional dependency behind the `cosmic` feature: `cargo build --workspace` pulls **0** libcosmic crates; `cargo run -p claudo-cosmic --features cosmic` builds the real app. Without the feature, the binary is a helpful stub.
- Verified: compiles + clippy-clean with the feature; gating confirmed via `cargo tree`. Live COSMIC render is for the user's machine (needs a COSMIC/Wayland session).

**M6: Polish (in progress)**
- Done: one persistent `claude` process per conversation; model menus listing the CLI's families by alias, so each follows its newest version (Opus by default); live thinking summaries; a stop button; subscription usage windows parsed (not displayed yet); parser unit tests; COSMIC tiles, several agents side by side (views keyed by conversation, a layout per window, so more windows are more layouts).
- COSMIC, following Claude Desktop's layout: markdown with highlighted code, message bubbles, expandable thinking and tool rows, a multi-line composer holding the conversation's model, thinking and permissions, a sidebar with New chat, Recents and Settings, a settings page, and hot reload (`cargo cosmic-hot`).
- Open: more windows (`view_window`, moving a tile between windows), showing the usage windows, attaching a project folder in COSMIC, attachments/images, conversation search, packaging (RPM/deb/AppImage, .msi, .dmg), autostart for daemon mode.

## 13. Open questions for review

All resolved in review (see "Decisions locked" at top): directory, name (`claudo`), egui-first, permission posture (prompt + auto toggle), no API-key fallback, distribution via private GitHub repo + build-from-source, conversation cwd strategy, subprocess model.

Nothing open. Plan is ready to build against.

## 14. Risks

- **CLI behavior/ToS change (no escape hatch)**: Anthropic could lock headless to `--bare` (drops subscription auth) or change `stream-json`. Since the API-key engine was dropped, there is **no fallback**: if this breaks, the app stops working until the CLI path is fixed or an API-key engine is added back. Accepted tradeoff for a subscription-only tool. Mitigation: isolate everything in `engine/cli.rs`, pin a tested CLI version range, parse defensively. This is the biggest strategic risk.
- **`stream-json` schema drift**: pin and test against a known CLI version; parse defensively.
- **Agent-shaped output**: not raw chat; may need prompt/tool steering to feel like a plain assistant.
- **Process overhead / latency**: addressed by the persistent process per conversation; its cost is one idle `claude` per recently used conversation.
- **Rendering quirks**: egui/iced render via wgpu (no webview flakiness); watch wgpu/Wayland edge cases on COSMIC.
- **wasm feature gaps**: no fs/subprocess in the browser: the web client must go through the daemon, which the architecture already forces.
- **Two frontends drifting**: mitigated by keeping all logic in `core`; frontends only render + emit.
