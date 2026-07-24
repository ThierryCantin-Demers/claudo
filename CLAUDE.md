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

One turn:

```bash
claude -p "<prompt>" --output-format stream-json --include-partial-messages --verbose \
  --permission-mode <default|acceptEdits|bypassPermissions> \
  [--resume <session-id>] [--model <m>] [--effort <low|medium|high|xhigh|max>]
```

Events consumed from the JSONL stream:

| Line | Meaning |
|---|---|
| `system` / `subtype: init` | carries `session_id` |
| `stream_event` -> `content_block_delta` -> `text_delta` | assistant text |
| `assistant` message, `tool_use` block | tool invoked |
| `user` message, `tool_result` block | tool returned (denials arrive here as errors) |
| `result` | turn complete; `usage` and `modelUsage[*].contextWindow` for context display |

Parse defensively by field extraction, not strict structs, so unrelated schema changes do
not break chat.

Transcripts live at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Locate one by its
`<session-id>.jsonl` filename rather than reversing the directory encoding. Relevant line
types: `user` (content is a plain string for prompts, or an array containing `tool_result`),
`assistant` (blocks `thinking`, `tool_use`, `text`), `ai-title`. Ignore the rest.

Use `--resume <session-id>` for continuation. `--continue` with `-p` can silently start a
new session.

**Per call permission prompts are not available.** In headless mode the CLI auto denies
tools that need approval, and emits no permission request. Live approve/deny would mean
replicating the Agent SDK's undocumented `initialize` + `can_use_tool` control protocol over
a persistent bidirectional session. This is deferred (M3b in PLAN.md). Permission profiles
are the supported mechanism.

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

**Fonts**: egui bundles Latin glyphs only, so the Japanese font is embedded via
`include_bytes!` and registered as a fallback. Do not switch to loading system fonts; that
breaks the web build and portability.

**libcosmic** is a git dependency pinned to a rev, optional behind the `cosmic` feature.
Keep it optional. API notes: `cosmic::app::{Application, Core, Task, Settings, run}`,
widgets under `cosmic::widget`, and `row()`/`column()` require children so use
`Row::with_children` / `Column::with_children`. Sidebar labels need
`Wrapping::None` or they overflow their button and overlap the next row. There is no runtime
window scale factor, so zoom scales text sizes only.

**Streaming feel**: the CLI delivers text in large chunks, so both frontends reveal it
gradually with a per word fade rather than dumping each chunk. If streaming looks wrong,
check the reveal animation before suspecting the backend.

**Dev builds optimize dependencies** (`[profile.dev.package."*"] opt-level = 3`) because
unoptimized text shaping made the cosmic frontend feel like it had a one second delay.

## Possible change: persistent CLI process

Today every turn spawns a fresh `claude -p` (spawn per turn, continuity via `--resume`).
Keeping **one long lived `claude` process per conversation** is possible using the
bidirectional streaming mode, with turns written as JSONL on a kept open stdin:

```bash
claude --input-format stream-json --output-format stream-json --verbose
```

This mode is confirmed to work. A user turn is one JSON line
(`{"type":"user","message":{"role":"user","content":[{"type":"text","text":"..."}]}}`)
and events stream back in the format `engine/cli.rs` already parses.

**It would cut latency, not tokens.** The Messages API is stateless, so every turn resends
the conversation plus system prompt and tool definitions regardless of process lifetime.
Prompt caching already absorbs the repeated overhead and works *across separate
invocations*, because the cache is server side and keyed on the prompt prefix. Observed:
about 12.2k `cache_creation_input_tokens` the first time, then about 5.9k
`cache_read_input_tokens` on a later invocation, billed at roughly a tenth of the price.

So the real wins are: no Node startup per turn (most of the 2-3s before first token), no
re-init of config, MCP servers and CLAUDE.md discovery. It does **not** avoid the ephemeral
cache TTL expiring during slow conversations.

To actually reduce tokens, shrink the context instead: fewer enabled tools, shorter
conversations rather than long resumed threads, a cheaper model for throwaway turns.

Design considerations if implemented:

- Process lifecycle: one child per open conversation, idle timeout, cleanup on close, and a
  cap so many conversations do not mean many live processes.
- Backpressure and interleaving: a persistent process must correctly attribute events to
  turns, and support cancellation mid turn.
- Fallback: keep spawn per turn as a fallback if the persistent process dies.
- Permission handling: the same bidirectional mode is the prerequisite for the deferred M3b
  per call permission prompts, so the two efforts overlap.

## Workflow

- The project is a git repo (`origin` is a private GitHub repo). Commit at stable points
  with a clear message.
- **Never push, open a PR, or create an issue without asking first.**
- Do not force push a branch that has an open PR.
- Keep comments minimal and only where they earn their place. Do not narrate what changed
  relative to previous code.
- Do not use em dashes in comments, docs, or anything else a human reads.
- Prefer verifying with the probe examples over asserting something works.
