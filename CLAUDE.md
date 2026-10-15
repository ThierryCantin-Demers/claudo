# Working on claudo

Guidance for an agent picking this project up. Read this before changing code.

## What this is

A native GUI over the **Claude Code CLI**. `core` spawns `claude -p` and parses its
`stream-json` output. The CLI owns auth and the agent loop, which is how the app runs on
the user's Claude subscription without touching tokens.

Read [README.md](README.md) for the feature list and [PLAN.md](PLAN.md) for design rationale.

## Architecture invariants

Do not break these without discussing it first.

1. **Frontends are dumb.** They render `ServerEvent`s and emit `Command`s. No conversation
   state, streaming reassembly, or tool orchestration in a frontend crate. If reasoning
   creeps into `frontends/*`, it belongs in `core`.
2. **Frontends never depend on `core`.** They depend on `protocol` and `transport` only.
   Breaking this makes the wasm build pull process and filesystem code.
3. **All CLI specifics stay in `core/src/engine/cli.rs`.** Flag names and `stream-json`
   shapes are the fragile part; isolate them so a CLI change touches one file.
4. **No API key path.** Subscription via CLI is the only engine, by deliberate choice.

## Commands

```bash
cargo build --workspace                      # everything EXCEPT libcosmic
cargo clippy --workspace --examples          # keep clean
cargo run -p claudo-egui --release           # native app

./scripts/build-web.sh                       # wasm bundle -> crates/frontends/egui/dist
cargo run -p claudo-daemon                   # serves bundle + WebSocket, prints token URL

cargo run -p claudo-cosmic --features cosmic --release   # COSMIC frontend
cargo build -p claudo-egui --target wasm32-unknown-unknown
```

The COSMIC UI can be driven without a window: `crates/frontends/cosmic/src/app/tests.rs`
builds the real view on a headless renderer, sends it events, applies the messages to
`update`, and keeps widget state between frames. Use it to check UI behaviour yourself,
and its ignored `snapshots` test (`cargo test -p claudo-cosmic --features cosmic --
--ignored`) to render PNGs into `target/snapshots` and look at them.

Verify changes with the probe examples (they hit the real CLI and cost tokens, so keep
prompts tiny):

```bash
cargo run -p claudo-core --example backend_probe
cargo run -p claudo-daemon --example ws_probe     # start the daemon with CLAUDO_TOKEN=test first
```

## Gotchas that have already cost time

**The cosmic binary is a stub without its feature.** `./target/debug/claudo-cosmic` built
without `--features cosmic` prints a message and exits. Always rebuild with the feature
before running or debugging it.

**Kill stray instances.** Launching the app repeatedly leaves windows around and you end up
testing a stale binary. Check and clean up:

```bash
ps aux | grep 'target/.*claudo' | grep -v grep
```

Note that `ps` on a backgrounded shell PID shows the wrapper shell, not the app.

**`ps -o pcpu` is an average since process start**, not instantaneous. Sample
`/proc/<pid>/stat` deltas for real CPU numbers.

**Cosmic window transparency is intentional.** It is the main reason a native COSMIC
frontend exists. Do not "fix" it by adding an opaque background.

**Do not read credential files.** Detect login with `claude auth status --json` only.

**Pro vs Max**: the CLI reports `subscriptionType: "pro"` for Max subscribers too. This is a
known Claude Code display bug. Never gate behaviour on it; only `loggedIn` matters.

**"ultracode" is not a CLI flag.** There is no `--ultracode` or `--fast`. Do not add a
toggle that pretends otherwise.

## CLI facts worth knowing

One process per conversation, turns written to stdin as JSONL
(`{"type":"user","message":{"role":"user","content":[{"type":"text","text":"..."}]}}`):

```bash
CLAUDE_CODE_SESSION_NAME=<conversation title> \
claude -p --input-format stream-json --output-format stream-json \
  --include-partial-messages --verbose --thinking-display summarized \
  --permission-mode <default|acceptEdits|bypassPermissions> \
  --settings '{"permissions":{"ask":[...]}}' --permission-prompt-tool stdio \
  --replay-user-messages \
  [--resume <session-id>] [--model <m>] [--effort <low|medium|high|xhigh|max>]
```

Each user line carries a `uuid`, and an `initialize` control request goes first. Its response
lists the models: claudo offers the family aliases (`opus`, `sonnet`, ...) labelled with
the version each runs today (`displayName`), so a choice follows new releases.

Every turn emits its own `system`/`init` and ends with its own `result`; the process then
waits for the next line. Closing stdin makes it finish the running turn and exit.
`--resume` keeps the session id, so a respawned process continues the same transcript.

`--thinking-display summarized` is not in `--help`. Without it print mode sends every
thinking block empty; with it the summary streams as `thinking_delta` and is saved in the
transcript. The model decides per turn whether to think at all, so many turns have none.

Events consumed from the JSONL stream:

| Line | Meaning |
|---|---|
| `system` / `subtype: init` | carries `session_id` |
| `stream_event` -> `content_block_delta` -> `text_delta` | assistant text |
| `stream_event` -> `content_block_delta` -> `thinking_delta` | thinking summary |
| `assistant` message, `tool_use` block | tool invoked |
| `user` message, `tool_result` block | tool returned (denials arrive here as errors) |
| `control_request` / `can_use_tool` | a call waits for approval; answered with a `control_response` |
| `command_lifecycle` | `state` `queued`/`started`/`completed` for the message with `command_uuid` |
| `user` with `isReplay` | a message starting its turn; one from another agent has `origin.name` and `origin.body` |
| `rate_limit_event` | `rate_limit_info.unifiedWindows` five-hour and seven-day `utilization` and `resetsAt` |
| `result` | turn complete; `usage` and `modelUsage[*].contextWindow` for context display |

Parse defensively by field extraction, not strict structs, so unrelated schema changes do
not break chat.

Transcripts live at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Locate one by its
`<session-id>.jsonl` filename rather than reversing the directory encoding. Relevant line
types: `user` (content is a plain string for prompts, or an array containing `tool_result`),
`assistant` (blocks `thinking`, `tool_use`, `text`), `ai-title`. Ignore the rest.

Use `--resume <session-id>` for continuation. `--continue` with `-p` can silently start a
new session.

**Approvals go through the control protocol on stdin.** Each process starts with
`--settings '{"permissions":{"ask":[...]}}'` (the `ASK_RULES` in `cli.rs`: `gh pr create`,
`gh issue create` and GitHub MCP's create tools) and `--permission-prompt-tool stdio`. The CLI
then sends `{"type":"control_request","request":{"subtype":"can_use_tool",...}}` for every
call that would prompt and waits for a `control_response`. Ask rules hold in every mode,
`bypassPermissions` included. Only requests with `decision_reason_type: "rule"` reach the user
as `ApprovalRequested`; the rest are denied with the message the CLI used before, so Safe mode
behaves as it always did. Without `--permission-prompt-tool stdio` the CLI denies on its own
and sends nothing; no `initialize` handshake is needed.

**Usage comes from the user's subscription pool**, the same quota as interactive Claude Code.
Each turn carries agent overhead (roughly 6-12k cache creation tokens on a fresh session).
Keep test prompts small.

## Toolkit notes

**egui 0.35** differs from older examples: implement `fn ui(&mut self, ui: &mut egui::Ui, ...)`
rather than `update`; `CentralPanel::show` takes a `&mut Ui`; panels are `egui::Panel::bottom`
/ `left` / `top` with `default_size`, not `TopBottomPanel`/`SidePanel`; use
`ui.selectable_label` rather than a `SelectableLabel` struct. Multiline newline insertion is
controlled by `TextEdit::return_key`, which is how Enter sends and Shift+Enter inserts a
newline.

**Selectable text**: iced's text and markdown widgets cannot be selected, so COSMIC
transcripts draw their text with `Selectable` (`src/app/selectable.rs`), a rich-text widget
on cosmic-text's hit testing and highlight runs, and render markdown through its `Viewer`.
New transcript text should go through it too, or it cannot be selected or copied.

**Fonts**: egui bundles Latin glyphs only, so the Japanese font is embedded via
`include_bytes!` and registered as a fallback. Do not switch to loading system fonts; that
breaks the web build and portability.

**libcosmic** is a git dependency pinned to a rev, optional behind the `cosmic` feature.
Keep it optional. Markdown and code highlighting come from its `markdown` and
`highlighter` features (iced's markdown widget). API notes: `cosmic::app::{Application, Core, Task, Settings, run}`,
widgets under `cosmic::widget`, and `row()`/`column()` require children so use
`Row::with_children` / `Column::with_children`. Sidebar labels need
`Wrapping::None` or they overflow their button and overlap the next row. There is no runtime
window scale factor, so zoom scales text sizes only.

**Streaming feel**: the CLI delivers text in large chunks, so both frontends reveal it
gradually with a per word fade rather than dumping each chunk. If streaming looks wrong,
check the reveal animation before suspecting the backend.

**Hot reload depends on `vendor/iced_core`.** libcosmic's iced keeps named widget states
in a thread-local, and a hot patch gets its own copy of every thread-local, so patched widgets
would not see the states the unpatched event loop stored. The vendored `iced_core` moves them
to a plain static behind its `hot` feature, which only `claudo-cosmic/hot` enables. When the
libcosmic rev changes, refresh the copy from that rev's `iced/core` and reapply the change in
`src/widget/tree.rs`.

**Named widget state.** libcosmic gives a widget with a custom id (`widget::Id::new`) its
state back by name every frame, wherever it moved. Two traps: a container reports its
content's id, not its own, and a scrollable reports a compound one, so neither name works;
and between direct siblings libcosmic finds a moved state by name but leaves it at its old
index, so the named widget must sit below the reordered parent's direct child. Tiles do
both through `CtrlPress`; `src/app/tests.rs` checks a dragged tile keeps its scroll. `cargo hot` only reacts to in-place writes, so `sed -i`, which replaces
the file, does not trigger a patch. New COSMIC subscriptions go through `named(...)`, not
`Subscription::run`: iced identifies a `run` subscription by its function's address, which
every patch changes, so it would restart (for the backend, mid-reply).

**Dev builds optimize dependencies** (`[profile.dev.package."*"] opt-level = 3`) because
unoptimized text shaping made the cosmic frontend feel like it had a one second delay.

## Persistent CLI processes

Each conversation has at most one live `claude` process (`core/src/live.rs`), driven by
`engine::Session`. Things to keep in mind when touching it:

- **Turns follow the CLI's message ids.** A turn belongs to the message whose
  `command_lifecycle` reports it `started`, and ends at its `result`. The CLI also starts
  turns itself, for another agent's message, and queues the client's messages behind
  them; those turns go to the `incoming` callback given at spawn, which the backend
  forwards like any other, starting with `Incoming`.
- **Agents are named after their conversation's title.** That is the name other
  sessions see in `ListAgents` and message with `SendMessage`. The `initialize` request
  makes the process visible at once instead of after its first turn, and a title
  change renames a live process with the `rename_session` control request. Only a
  conversation with a live process can be messaged.
- **Flags are per process.** Profile, model and effort are command line flags, so a turn
  with different ones replaces the process with a `--resume` one. Only an idle process is
  replaced.
- **Closing means closing stdin, never killing.** An idle process exits at once; one in
  the middle of a turn finishes it first, so a dropped daemon connection does not cut
  off an agent mid-edit. Processes idle for 30 minutes are closed this way.
- **stderr is drained continuously**, keeping the last lines for the error message. A
  long-lived process blocks once an unread pipe fills.
- There is no cap on live processes beyond the idle timeout.

**This cuts latency, not tokens.** The Messages API is stateless, so every turn resends
the conversation plus system prompt and tool definitions regardless of process lifetime.
Prompt caching already absorbs the repeated overhead and works *across separate
invocations*, because the cache is server side and keyed on the prompt prefix. The win is
no Node startup per turn (most of the 2-3s before first token) and no re-init of config,
MCP servers and CLAUDE.md discovery. It does **not** avoid the cache TTL expiring during
slow conversations. To reduce tokens, shrink the context: fewer enabled tools, shorter
conversations, a cheaper model for throwaway turns.

The open stdin also carries approval answers and interrupts.

**Stopping a turn** writes `{"type":"control_request","request":{"subtype":"interrupt"}}`. The
turn still ends with a `result`, flagged `is_error` with `terminal_reason` `aborted_streaming`
(mid-reply) or `aborted_tools` (during a call or an approval); `core` reports any `aborted_*`
turn as a normal completion. A pending approval is withdrawn first with a
`control_cancel_request`. The process stays up for the next turn.

## Workflow

- The project is a git repo (`origin` is a private GitHub repo). Commit at stable points
  with a clear message.
- **Never push, open a PR, or create an issue without asking first.**
- Do not force push a branch that has an open PR.
- Keep comments minimal and only where they earn their place. Do not narrate what changed
  relative to previous code.
- Do not use em dashes in comments, docs, or anything else a human reads.
- Prefer verifying with the probe examples over asserting something works.
