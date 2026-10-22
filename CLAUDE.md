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
cargo xtask --help                           # every project command (xtask/)
cargo build --workspace                      # everything EXCEPT libcosmic
cargo clippy --workspace --examples          # keep clean
cargo run -p claudo-egui --release           # native app

cargo xtask web                              # wasm bundle -> crates/frontends/egui/dist
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
cargo run -p claudo-transport --example remote_probe
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

**One claudo per build.** Launching the binary that is already running opens a new window
in it and exits (`src/app/instance.rs`, a socket in `$XDG_RUNTIME_DIR`). Any other binary,
including one rebuilt since, runs beside it in one window and saves no window layout, so a
dev run neither reopens the user's windows nor overwrites them.

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

Each user line carries a `uuid`, and its `content` may hold API blocks besides text:
`image` (base64), and `document` with a base64 PDF or a `text` source and a `title`. The CLI
takes a PNG of several MB as is, and the transcript keeps the blocks inline, where the loader
turns them back into files.

An `initialize` control request goes first. Its response lists the models: claudo offers the family aliases (`opus`, `sonnet`, ...) labelled with
the version each runs today (`displayName`), so a choice follows new releases.

A slash command is sent as an ordinary message (`/compact keep it short`). A local one
such as `/context` streams no text: its output is the `result` field, which the backend
passes on as the reply. The transcript records the command as `<command-name>` and
`<local-command-stdout>` user lines, `isMeta` caveats and an `isCompactSummary` summary,
which the loader turns back into what was typed, a reply and a compaction. The
`initialize` response lists every command (`commands`: `name`, `description`,
`argumentHint`) for the message box's `/` menu.

A `remote_control` control request (`enabled`, `name`) puts the session on claude.ai/code
and in the Claude app, from print mode too and in a folder never trusted; its answer
carries `session_url`, and `system` / `bridge_state` lines (`ready`, `connected`, `failed`)
follow the connection. The Remote Control setting sends it to every process.

With `promptSuggestions: true` in `initialize`, a turn the user started is followed, a
few seconds after its `result`, by `{"type":"prompt_suggestion","suggestion":...}`: the
CLI's guess at the next message, from a forked request over the cached conversation (one
more request per turn, mostly cache reads). It stays silent when the next step is not
obvious, early in a conversation, when the cache is cold, and near rate limits. The CLI
reads the option once: `set_prompt_suggestions_paused` is acknowledged but ignored behind a
feature flag (`tengu_chomp_sable`), and so is skipping turns the user did not start. So
claudo starts a process with suggestions only when its conversation is on screen
(`Command::OnScreen`) and the setting is on, treats that like a flag (a message replaces
an idle process that suggests otherwise, such as one an older build started), closes idle
processes when the setting changes, and sends the pause anyway for when it is honored. `examples/suggestion_probe`
checks it.

The Debug log setting (`crate::debug`) appends what claudo does with each agent to
`$XDG_STATE_HOME/claudo/debug/claudo.log` (spawns and replacements with their suggestion
state, what is on screen, turn ends, suggestions received) and starts each new agent with
`--debug-file` beside it. The CLI's debug log shows its `source=prompt_suggestion` request
and the forked agent's usage, but not why it dropped a guess: that goes only to telemetry.

A `get_usage` control request answers with `rate_limits.five_hour` and `.seven_day`
(`utilization` in percent, `resets_at` as RFC 3339) without a turn, so it costs no tokens;
`cli::usage` asks a short-lived process for it.

Every turn emits its own `system`/`init` and ends with its own `result`; the process then
waits for the next line. Closing stdin makes it finish the running turn and exit.
`--resume` keeps the session id, so a respawned process continues the same transcript.

`--thinking-display summarized` is not in `--help`. Without it print mode sends every
thinking block empty; with it the summary streams as `thinking_delta` and is saved in the
transcript. The model decides per turn whether to think at all, so many turns have none.
The summary arrives in one burst when the thinking block ends, so a long think streams
nothing until then; `system` / `thinking_tokens` only accompanies those deltas.

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
| `user` with `isReplay` | a message starting its turn; one from another agent has `origin.name` and `origin.body`, one the user typed in the Claude app (Remote Control) or a terminal has `origin.kind: "human"`, and the client's own have no `origin` |
| `system` / `status` with `status: "compacting"` | a compaction is running |
| `system` / `compact_boundary` | it ended: `compact_metadata` `trigger` (`manual`/`auto`), `pre_tokens`, `post_tokens` |
| `user` with `isSynthetic` | the compaction's summary, as a string |
| `system` / `task_started` | a call moved to the background (`is_backgrounded`), by `tool_use_id` |
| `system` / `task_notification` | a background call ended: `status`, `summary`, `output_file`; may arrive between turns |
| `rate_limit_event` | `rate_limit_info.unifiedWindows` five-hour and seven-day `utilization` and `resetsAt` |
| `assistant` with no `parent_tool_use_id` | its `message.usage` is one request's context; the turn's last one is the context shown |
| `assistant` / `user` with a `parent_tool_use_id` | a subagent's calls and results, by the Agent call that started it; its text is not streamed |
| `system` / `api_retry` | a failed request will be retried: `attempt`, `max_retries`, `error`, `error_status` |
| `result` | turn complete; `modelUsage[*].contextWindow` for the window. Its `usage` sums every request of the turn, so it is not the context |

Parse defensively by field extraction, not strict structs, so unrelated schema changes do
not break chat.

A message sent while a turn runs that joins it at its next step is not a `user` line in
the transcript but an `attachment` line (`attachment.type: "queued_command"`, its text in
`prompt`, with the `origin` a user line would carry); the loader reads it as the user line
it stands for, in the turn it joined. A long conversation opens with its last page of
entries (`History.earlier` counts the rest, cut where a turn starts) and `LoadEarlier`
pages back through it, since all of one at once took seconds to send to another computer
and lay out.

Transcripts live at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. Locate one by its
`<session-id>.jsonl` filename rather than reversing the directory encoding. Relevant line
types: `user` (content is a plain string for prompts, or an array containing `tool_result`),
`assistant` (blocks `thinking`, `tool_use`, `text`), `ai-title`. Ignore the rest.

Use `--resume <session-id>` for continuation. `--continue` with `-p` can silently start a
new session. `--resume` finds a session from any folder.

**Sessions other apps started.** Claude Desktop's Code sessions and terminal ones are
transcripts in the same place, told apart by `entrypoint` (`claude-desktop`, `cli`; every
`-p` run, claudo's own included, is `sdk-cli`). They also carry `custom-title` (a title the
user gave) and `relocated` (`relocatedCwd`, the worktree the session moved into); a session
that moved leaves a copy of its transcript behind, so the newest `<session-id>.jsonl` is the
live one. claudo continues one with `--resume <id> --fork-session`, which copies the history
into a new session under the new process's folder and leaves the original untouched; until
that first turn the conversation's `session_id` is still the other app's (`imported_from`),
so nothing may delete it.

**Subagents.** An Agent call's subagent streams its calls live, in the foreground or the
background, and a background one keeps streaming after the turn that started it ended,
which the engine passes on between turns. The main transcript keeps none of them: each
subagent writes `<session-id>/subagents/agent-<id>.jsonl`, beside an `agent-<id>.meta.json`
whose `toolUseId` names the Agent call, and the loader places its calls after that call.
A subagent `SendMessage` resumes, as after a usage limit, keeps its task id and its calls
still name the Agent call, but its `task_started` and `task_notification` name the
`SendMessage` call; claudo puts them on the Agent call, which `toolUseId` names after a
restart too, so the subagent shows running again where it was.

**A forked slash command streams nothing while it runs.** A skill with `context: fork`, such
as `/code-review` in an older process (see below), starts with `system` / `task_started` (`task_type: "local_agent"`, no
`tool_use_id`) and ends with `task_notification`; only then does the CLI replay the
subagent's messages (`parent_tool_use_id: "forked-command-<name>"`), send the turn's `init`
and its `result`. The subagent writes its transcript live to
`<session-id>/subagents/agent-<task_id>.jsonl`, which the engine follows for the calls in
the meantime, dropping the replayed ones by call id. The main transcript keeps only the
prompt and a `system` / `local_command` line with the result.

**Code reviews.** Every process starts with `CLAUDE_CODE_REPORT_FINDINGS=1`, which is not
documented. With it, `/code-review` runs in the conversation rather than forked, and hands
its findings over in a `ReportFindings` call: each with `short_summary` (at most 60
characters), `category` (`correctness`, `reuse`, `test-coverage`, ...; a low effort review
gives none) and `verdict` (`CONFIRMED`, `PLAUSIBLE`), and `outcome` when reported again
after `--fix`. The CLI refuses a call that breaks the schema, such as a longer title, and the
reviewer calls again, so `cli::reported_review` becomes a `Review` only once its result is
not an error, live and from the transcript, and the call itself is never shown. A
`correctness` or `security` finding is a bug, which COSMIC lists first.
`examples/review_probe` checks it.

Without the variable, as in a process an older build started, the forked subagent answers
with a JSON array of findings, which the CLI writes into its result as markdown:
`13 findings, most severe first:`, then items like ``1. `src/a.rs:12`: summary``, each with
`Failure scenario: ...` indented under it, and any text the reviewer wrote before; it drops
every field but the file, line, summary and failure scenario. `cli::review` reads that list
back into a `Review`, live and from the transcript; text it cannot read stays a plain reply.

**Approvals go through the control protocol on stdin.** Each process starts with
`--settings '{"permissions":{"ask":[...]}}'` (the `ASK_RULES` in `cli.rs`: `gh pr create`,
`gh issue create` and GitHub MCP's create tools) and `--permission-prompt-tool stdio`. The CLI
then sends `{"type":"control_request","request":{"subtype":"can_use_tool",...}}` for every
call that would prompt and waits for a `control_response`. Ask rules hold in every mode,
`bypassPermissions` included. Only requests with `decision_reason_type: "rule"` reach the user
as `ApprovalRequested`, and `"subcommandResults"` ones, which the CLI sends for a compound
command like `cd x && gh issue create`, when a part of the command matches a rule; the rest are denied with the message the CLI used before, so Safe mode
behaves as it always did. Without `--permission-prompt-tool stdio` the CLI denies on its own
and sends nothing; no `initialize` handshake is needed. claudo answers "Allow always" itself:
the scope (the rule matched, or the exact call when something else may have asked) is saved
on the conversation and given to each process it spawns, which then allows it without asking,
since an allow rule cannot override an ask rule.

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
New transcript text should go through it too, or it cannot be selected or copied, and
web addresses in it become links (`src/app/links.rs`: markdown only links them inside `<>`). It also
draws furigana (`src/app/furigana.rs`): a reply's `<ruby>` elements reach it as Unicode's
interlinear annotation characters, the only form the markdown parser keeps, and it lays out
the words alone and draws each reading over them. cosmic-text has one line height per
paragraph, so every line of a paragraph with readings is spaced for them (the paragraph is
drawn higher by half that spacing, so its first line stays level with a list number), and a copied
selection holds the words without their readings. Ruby inside code, inline or fenced, stays
as written. Tables are its own too: iced's sizes a column by what its cells measure,
and a block, which fills the width it is given, measures nothing, so claudo's fit the reply's
width and share it by each column's longest cell.

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
`src/widget/tree.rs`. `cargo hot` only reacts to in-place writes, so `sed -i`, which replaces
the file, does not trigger a patch. New COSMIC subscriptions go through `named(...)`, not
`Subscription::run`: iced identifies a `run` subscription by its function's address, which
every patch changes, so it would restart (for the backend, mid-reply).

**Named widget state.** libcosmic gives a widget with a custom id (`widget::Id::new`) its
state back by name every frame, wherever it moved. Two traps: a container reports its
content's id, not its own, and a scrollable reports a compound one, so neither name works;
and between direct siblings libcosmic finds a moved state by name but leaves it at its old
index, so the named widget must sit below the reordered parent's direct child. Tiles do
both through `Intercept`; `src/app/tests.rs` checks a dragged tile keeps its scroll. Inside a
tile, a named widget whose index changes gets an empty state and panics ("Downcast on
stateless state") even wrapped in a container, so keep its index fixed: the message box puts
attachments after the editor, not before. A slot above it that changes widget type also
loses the editor's state, its focus included, so the label over the message box is always a
container and only its content changes.

**Several windows.** Each window has a `WindowLayout` keyed by its `window::Id`; messages
from a window's view and keys pressed in it arrive as `Message::In(window, ..)`. libcosmic
draws its header bar and frame for the main window only, so `view_window` draws them for the
others, and a context menu opens over the first window unless given `.window_id(w)`. Closing
the main window hands its role to another with `core.set_main_window_id`, which keeps
libcosmic from exiting. Bring a window forward with `raise`, never `window::gain_focus`
alone, which winit's Wayland backend leaves empty. A tile dragged past its window's edge becomes a Wayland drag and
drop (`app/tile_drag.rs`): a window learns nothing of the cursor outside it, so only the
compositor can say which window the tile was dropped on. A window cannot place itself on
Wayland either, so `app/placement.rs` watches which workspace each window is on through
COSMIC's toplevel protocols, on a Wayland connection of its own, saves it as soon as one
moves, and sends the reopened windows back. A launch leaves the main window where claudo
was launched from, with the windows that shared its workspace, and sends back only those
left apart; Update saves `restarting` in the window state first, so the new build sends
back every window. That list knows windows by title alone, so
each window is titled with its focused conversation, made unique. Where a window sits on
its workspace is the compositor's to decide and is not restored.

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
- **A message sent mid-turn joins it.** The CLI reports it `queued`, then at the turn's
  next tool result replays it and reports it `started` while the turn still runs; both
  messages end with that turn's one `result`. With no step left, it waits and starts the
  next turn. Either way the engine sends `Delivered` (`joined` or not), which the
  frontend uses to move the message from its queue into the transcript. A message sent
  while busy keeps the running process's flags, since it may join its turn.
- **A background command's end starts a turn of its own.** The CLI tells the model in
  a turn that echoes no message, so the engine opens it with a `Notice` carrying the
  CLI's summary. The transcript keeps that message as a `<task-notification>` prompt
  (`origin.kind: "task-notification"`), which the loader turns back into the notice.
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
- **A call in the background keeps its process alive**, stdin closed or not, until it
  ends. Only that process reports its end, so one running a call in the background is
  never closed for idling nor replaced for new flags (a message keeps the flags it has).
  The transcript cannot tell whether such a call still runs, so a reopened conversation
  takes that from its process (`LiveSessions::in_background`), and one with no process of
  this claudo's shows the call as ended. After a restart the new build reports each call an
  adopted process still runs in the background (`ServerEvent::Background`), which opens its
  conversation in COSMIC, and a view whose commands still run is kept without a tile.
- **Restarting into a new build hands every process over.** Once the binary claudo runs
  from is replaced (`/proc/self/exe` reads as deleted), the sidebar offers an Update
  button beside Settings, which execs the new build in the same process, so each `claude` child keeps its
  pid and pipes. `LiveSessions::exec` holds every reader's `reading` lock, writes each
  session's state to `$XDG_RUNTIME_DIR/claudo-handover-<pid>.json` and clears FD_CLOEXEC on
  its pipes; the new build's `Backend::new` adopts them, says `Working` for turns in flight
  and asks pending approvals again. A build that cannot read the file closes the pipes
  `CLAUDO_HANDOVER_KEPT` lists, so the agents finish their turn alone. The reader reads and
  handles output only while holding `reading`, and reaps by pid with `waitpid`: without
  either, a handover loses a half-read line or leaves an adopted process unreaped.
  Messages not yet taken, queued in the CLI or waiting for another app's agent, go over
  with their text and files, and the new build shows them again (`ServerEvent::Queued`).
  `examples/handover_probe` and `restart_probe` check it on the real CLI, and
  `handover_cases_probe` a pending approval, a queued message, a background command and
  a forked command's subagent.
- **Several frontends share one backend.** The app's own window is `clients::LOCAL`, and
  claudo on another computer (`claudo-cosmic --on <host>`) is another client: everything
  that happens goes to each (`Clients::send`), an answer only to the client that asked
  (`History`, `Created`, `Status`, `Folders`, ...), and a message one client sends reaches
  the others as `Queued`, so their `Delivered` finds it. A client arriving hears what runs
  (`Backend::announce`: `Working`, approvals, queued messages, background calls), and an
  approval one answers is `Answered` for the rest. The suggestions' on-screen set is every
  client's.
- **Remote frontends come over ssh.** `transport::spawn_backend(true)` serves the backend on
  `$XDG_RUNTIME_DIR/claudo-backend.sock` (mode 0600, unless another claudo there already
  does; a dev build beside does not serve), one JSON line per `Command` in and per
  `ServerEvent` out. `claudo-cosmic --bridge` joins that socket to its stdin and stdout,
  and `transport::connect_remote` runs it with `ssh -T -o BatchMode=yes <host>`, so ssh
  brings the login and encryption. The transport asks for `Status` itself, says
  `Connection { connected }` as the link comes and goes, tries again with a backoff, and
  drops commands while it is down; COSMIC asks again for everything it shows once
  connected, holds messages in their box meanwhile, keeps its own instance socket and
  window state per host (`remote::scoped`), types the folder path rather than opening a
  picker, and Update just re-execs it. The bridge copies with a plain read and write loop:
  `io::copy`'s kernel copy between the socket and the pipe now and then held a reply back
  for good. `transport/examples/remote_probe` checks it on the real CLI.
- **One agent per conversation, whichever claudo runs it.** A process spawned for a
  conversation inherits its lock (`$XDG_RUNTIME_DIR/claudo-agents/<conversation>.lock`, an
  flock whose file holds the agent's pid) and keeps it as long as it runs, whatever becomes
  of the app that started it; the CLI does not pass it on to the commands its tools run. A
  message to a conversation whose lock another process holds, such as an agent a killed
  claudo left running or another claudo's, waits for it to end (`ServerEvent::Waiting`),
  and `Command::StopStray` ends it and what it started. While it sits idle, no turn and
  nothing in the background, the agent's app keeps `<conversation>.idle` beside the lock,
  so the waiting app says whether stopping it loses work; the next agent to take the lock
  removes one left over. Replacing an idle process for new flags waits for it to exit
  first. `examples/stray_probe` checks it on the real CLI.
- **Several claudo instances share `conversations.json`.** Each writes the whole list, so a
  save first takes in what another wrote: the conversations it added, and its version of
  each one this instance has not changed since it last read or wrote the file. Startup takes
  back any scratch conversation whose folder and transcript remain but which the list lost.
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
