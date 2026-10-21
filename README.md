# claudo

A native Claude client, without Electron, that runs on your Claude subscription by driving
the Claude Code CLI.

## Run

You need Rust and the [Claude Code CLI](https://code.claude.com), logged in with your own
subscription:

```bash
claude auth login
```

On a new machine, `cargo xtask setup` installs the tools the commands below need and prints
the system packages still missing. Every command checks for what it needs before it starts.

### COSMIC

```bash
cargo xtask install
```

That installs `claudo-cosmic` into `~/.cargo/bin` with a Claudo entry and icon in the app
launcher; `cargo xtask uninstall` removes all three. To run it from the checkout instead:
`cargo xtask cosmic --release`.

### egui (Linux, macOS, Windows)

```bash
cargo xtask egui
```

### Browser

Build the bundle and serve it:

```bash
cargo xtask web
cargo xtask daemon
```

Open the URL the daemon prints; it carries the auth token. `CLAUDO_PORT` and
`CLAUDO_TOKEN` pin the port and token.

## Features

- Conversations in a sidebar, read from Claude Code's own transcripts, so they are shared with the CLI.
- Send images, PDFs and text files with a message: COSMIC pastes an image from the clipboard with Ctrl+V, picks files with the paperclip, and takes files dropped from a file manager on a conversation's tile (as a copy, never a move), and keeps the files of an unsent message across restarts like its text; egui picks files with Attach. Click an image, sent or waiting, to open it over the window: the scroll wheel zooms around the cursor, a drag moves it, Escape closes it.
- Continue a session started in Claude Desktop or a terminal (COSMIC: right-click New chat, Continue a session; egui: Sessions…). Its first message forks it, so the original stays as it was.
- Streaming replies you can stop, with Claude's thinking and tool calls shown as collapsible rows.
- COSMIC: send a message while Claude works. It waits under the reply and reaches Claude at its next step, as in the terminal, or starts the next turn if Claude is already finishing.
- COSMIC: tool calls made one after another fold into one row ("6 calls · Bash ×6"), which opens to each call; while it runs, its newest call shows below it.
- Code reviews (`/code-review`) show as a list of findings, each on two lines: its rank, a short title, its category and whether it went unconfirmed, then its file and line, with what is wrong and its failure scenario folded under it. Bugs come first, each keeping the reviewer's rank.
- COSMIC: links open in the browser, whether written as markdown or as a bare address, in replies, your own messages and tool output.
- COSMIC: one claudo at a time. Launching it again while it runs opens a new window with a new chat in the running one.
- Never two agents on one conversation: a message to one whose agent another claudo runs, or a killed claudo left running, waits for it to finish; COSMIC says which process it waits on, whether it is working or idle, and offers to stop it.
- COSMIC: update without stopping agents. Once a new build is installed or rebuilt over the running one, an Update button beside Settings restarts into it, and every agent keeps working through the restart, mid-turn included, with its pending approvals and queued messages. A closed sidebar shows a dot on its toggle when an update is ready.
- Permission profiles: Safe, Accept edits, Full auto.
- Remote Control (COSMIC: Settings): your agents also show in the Claude app and on claude.ai/code, where you can follow them and message them.
- Model, thinking and permissions per conversation. The defaults in Settings apply to new conversations; in COSMIC a `*` after a value marks a choice that differs from them.
- COSMIC suggests what to send next once a turn you started ends, like Claude Desktop: the guess shows greyed in the empty message box and the right arrow key takes it. Only an agent started while its conversation is on screen makes them, since each costs a request over the cached conversation; Settings turns it off.
- A debug log (COSMIC: Settings) records what claudo does with each agent, such as whether it suggests and what it suggested, beside the Claude Code CLI's own debug log for each agent started while it is on.
- Each turn ends with when it started and how long it took; while one runs, COSMIC shows its time so far and says when the API is being retried.
- Context usage for the last turn, in the header, and in COSMIC also as a ring by the model picker: green, then yellow from half full and red from 80%. Past 500k tokens of context, COSMIC offers to compact the conversation; compactions, automatic or asked for, show where they happened with their summary.
- COSMIC: a `/` menu of the CLI's slash commands (built in, skills, plugins) as you type one; `/clear`, `/rename`, `/model` and `/effort` act on claudo's own conversation, title and settings.
- COSMIC: your plan's usage at the foot of the sidebar, the five-hour session and the week, with when each resets, when it was last updated, and a button to refresh it. With the sidebar closed, both show as small meters in the header bar, with when each resets on hover.
- A scratch working directory per conversation, or a project folder. COSMIC offers the folders you worked in lately, Claude Code's included, on a line under the message box's controls until the first message, and names the folder among them after; right-click New chat for New chat in folder…. egui: Folder….
- Zoom with Ctrl+scroll or Ctrl+`+`/`-`/`0`.
- COSMIC: several agents side by side in tiles (below).
- COSMIC: the Commands panel (terminal button in the header bar; drag its edge to resize) lists the shell commands of every open agent, grouped by conversation, running ones first and background ones included, plus any agent still running one after its tile closed; each row shows how long it ran, counting up while it runs, and finished ones are grayed and folded until opened; a row expands to the whole command and its output. A subagent is one row there for as long as it works, with how many calls it made and its latest; opened, it lists its calls and its report. In the conversation, its calls fold under the Agent call that started it, also after a reload and while a background one works on after its turn.
- COSMIC: furigana. Readings Claude writes as HTML ruby (`<ruby>漢字<rt>かんじ</rt></ruby>`) show small above their words.
- COSMIC: select any text in a conversation, across messages, and copy it with Ctrl+C; double-click selects a word, triple-click a paragraph, and a selection also fills the primary selection for middle-click paste.

### Tiles (COSMIC)

| Do | How |
|---|---|
| Open a conversation in the focused tile | Click it; one already open is focused instead, its window brought to the front |
| Open it in a new tile at the right | Shift+click, or right-click → Open in new tile |
| Open it in a new window | Alt+click, or right-click → Open in new window |
| New conversation | New chat or Ctrl+N; Shift+click or Ctrl+Shift+N for a new tile, Alt+click, Ctrl+Alt+N or right-click → New chat in new window for a new window |
| Resize tiles | Drag the line between two tiles |
| Move a tile | Drag its title or Ctrl+drag anywhere in it, right-click its title for Move left/right, or Alt+Shift+Left/Right |
| Focus the previous or next tile | Alt+Left/Right |
| Close a tile | Its ✕, or Ctrl+W; a window other than the first closes with its last tile |
| Move a tile to its own window | Right-click its title → Move to new window, or Alt+click it in the sidebar |
| Get your windows back | Launched, claudo opens where you are, with the windows that were beside its main one, and sends a window you kept on another workspace back there; after an update every window reopens where it was. Each is titled with its focused conversation |
| Move a tile to another window | Drag it there by its title (or Ctrl+drag), or right-click its title → Move to window N; dropped outside every window, it gets its own |
| Rename a conversation | Click its title, or right-click it in the sidebar → Rename; Enter keeps it, Escape cancels |
| Archive a conversation | Right-click it in the sidebar → Archive; it moves under Archived at the end of the list, kept whole. Unarchive, or a new message in it, brings it back |
| Delete a conversation | Right-click it in the sidebar → Delete…, then confirm |
| Delete or archive several at once | Ctrl+click them in the sidebar (or right-click → Select), Shift+click to pick a run of them, then Delete… or Archive at the top of the sidebar; Escape lets go |
| Reuse an earlier message | Up in an empty message box, again for older ones; Down goes back toward an empty box, Escape empties it. An accent border and a label mark a message already sent, and editing it makes it a new draft |
| Take back an edit | Ctrl+Z in the message box undoes a paste, a pasted or attached file, or the last word typed; Ctrl+Shift+Z or Ctrl+Y redoes |
| Take the suggested next message | Right arrow in the empty message box that shows it |
| Scroll a conversation | Scrolled up, a round button at the bottom takes you back down; PageUp/PageDown by a page; Home/End to the top or bottom, or Ctrl+Home/End while the message box holds text; a middle click scrolls toward the pointer, faster the farther it goes, until the next click or until you let go after moving |

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
- Opening an issue or a pull request (`gh` or GitHub's MCP server) always waits for your approval, in every permission profile, until you allow it always for that conversation. Everything else follows the profile.
- Each agent is named after its conversation's title, and renaming the conversation renames it, so your agents, and Claude Code sessions, can find each other with `ListAgents` and talk with `SendMessage`. A message one receives shows as "From <agent>" and runs a turn as if you had sent it. Only a conversation with a running `claude` process (used in the last 30 minutes) can receive one.
- claudo keeps its data in `~/.local/share/claudo/`. Message history stays in `~/.claude/projects/`.
- Two claudo windows at once, such as the installed app and a dev build, keep each other's conversations, but each runs its own agents: send to a conversation from one of them only.

## Development

```bash
cargo xtask --help          # every command
cargo build --workspace     # everything except COSMIC
cargo xtask lint            # clippy on everything, COSMIC included
cargo xtask test            # every test, COSMIC's headless UI tests included
cargo xtask probe live_probe # probes hit the real CLI and cost tokens
```

`cargo xtask probe` alone lists the probes. The commands live in `xtask/`.

### Live reload (COSMIC)

```bash
cargo xtask cosmic --hot
```

It needs the dev build. A release build loads each patch but keeps running the old
view code, which the optimizer inlined into callers the patch does not replace.

Saving a file in `crates/frontends/cosmic/src/` patches the running window in about two
seconds, keeping its state. Changing a type (a new `Message` variant or field) or another
crate still needs a restart.

## License

MIT, see [LICENSE](LICENSE). The bundled font is BIZ UDPGothic, under the SIL Open Font License (`assets/fonts/OFL.txt`).
