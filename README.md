<p align="center">
  <img src="packaging/icon-1024.png" width="128" height="128" alt="Deck">
</p>

<h1 align="center">Deck</h1>

<p align="center">
  <strong>A workspace for coding with agents.</strong><br>
  Your terminals, a browser and your coding agents in one native Mac window.
</p>

<p align="center">
  <img alt="macOS 13+" src="https://img.shields.io/badge/macOS-13%2B-000000?logo=apple&logoColor=white">
  <img alt="Version 0.1.0" src="https://img.shields.io/badge/version-0.1.0-000000">
  <img alt="MIT license" src="https://img.shields.io/badge/license-MIT-000000">
</p>

---

Deck puts the things you use all day while working with AI agents side by side. Talk to Claude Code, Codex, Gemini, Grok or OpenCode in a chat. Run any agent's own interface in a real terminal, including Cursor Agent, Amp and Aider. Preview your app in a browser right next to them. Hand an agent a task on its own branch, and review and merge its work when it's done, without leaving the window.

Agents can drive Deck too. From any chat or terminal they can open terminals, start your dev server, open pages and read what's on screen, so "start the app and check the page" just works.

## Features

### Chat with any agent

- **One window, every agent.** Chat with Claude Code, Codex, Gemini CLI, Grok and OpenCode, whichever you have installed, and switch agents in the middle of a thread. Cursor Agent, Amp and Aider run in their own terminals.
- **A control chat** pinned at the top, always there. It can run Deck for you: open tabs, split panes, start servers, open pages.
- **Threads you can read at a glance.** Work folds into a short summary ("Worked for 4s · Ran 2 commands") that opens into every step: the commands run and their output, the files read and edited, with diffs.
- **Approvals where you are typing.** When an agent asks to run a command or edit a file, the request appears above the composer. Answer with a click or ↑↓⏎, or just type what it should do instead. Questions from the agent show their options the same way.
- **Choose how much it may do:** Ask first, Accept edits, or Full access, per thread, from the access menu.
- **Pick the model** per thread: Opus, Sonnet or Haiku for Claude, Terra or Luna for Codex, Pro or Flash for Gemini, any of OpenCode's. The agent starts as soon as you click into the box, so the first reply does not wait for it to boot.
- **See what it costs.** The status bar shows Claude's 5-hour limit and the conversation's context, the same numbers Claude Code shows; click it for the weekly limits with when they reset, this conversation's tokens and cost, today's totals and Codex's limits.
- **Talk while it works.** Messages you send mid-turn reach the agent as soon as its current step finishes. ⌘⏎ sends one right away. Esc stops the turn.
- **Prompt rewriting.** A quick Claude run (Haiku by default) turns what you typed or dictated into a clear, complete prompt, using the thread's task and the agent's last reply to work out what "it" and "that" mean. ⏎ rewrites and sends, ⌃O rewrites into the box without sending, ⌃Z brings back your own words. Short replies, `/` commands and `!` commands go as typed. In Settings you can have it show the rewrite first, pick the model, or turn it off.
- **Turns that fail come back by themselves.** On an API or network error Deck says "continue" after 5s, 10s, 20s… up to five minutes, eight tries; a usage limit waits until it resets; a login or billing problem is shown to you instead. Claude Code stuck in its own retry wait in a terminal gets the same nudge.
- **Attach anything.** Paste a screenshot, drop files in, or `@` a file from the project. Click an attachment to preview it in Deck.
- **Undo a turn.** Every message you send in a git project saves the files as they were, out of your branch's way. Right-click your message to go back to before it, files and chat, or to edit it and go on from there.
- **Select and copy** any part of a conversation, across replies, lists, tables and code.
- **Threads last.** Transcripts, drafts and attachments survive a restart, and Claude resumes its own session.

### Tasks: agents on their own branch

- **Start a task from ⌘K.** Describe what you want ("add dark mode to settings") and pick *Start a task*. Deck makes a fresh branch and a separate checkout for it, then starts an agent working on it there.
- **Run several at once.** Each task has its own folder and branch, so agents never step on each other or on the checkout you're working in.
- **Agents can hand out tasks too.** An agent working on a big job can split it into sub-tasks, each with its own agent on its own branch. Deck tells it when each one finishes, so it can review the changes and merge them. Ask the control chat to "split this into tasks" and watch them run in parallel.
- **Watch them in the sidebar.** Each task shows what its agent is doing right now, or how many files and lines it has changed.
- **Preview it running.** Preview starts the task's dev server from its own checkout on a port of its own and opens the page beside the review.
- **Review, then decide.** The review shows every change file by file. Then:
  - **Merge** squashes it into your branch as one commit.
  - **Open PR** pushes the branch and opens a pull request.
  - **Discard** throws it away.
  - A terminal opens in the task's folder whenever you want to run the tests yourself.

### Terminals

- **Fast and faithful.** True colour, wide characters and emoji, mouse support for TUIs, IME input, smooth scrolling, find in scrollback.
- **Made for agent CLIs.** Paste a screenshot or drop files and their paths go in, so Claude Code and Codex attach them. ⌘-click a link to open it beside the terminal.
- **Knows what's running.** Tabs are named after the program's own title (Claude Code's current task, `vim file.rs`) and the project folder they're in.
- **Commands as blocks.** Each command and its output is a block with its exit status and time, Copy, Rerun and Fix with agent; ⌘↑ / ⌘↓ jump between them. A failed one is marked in red.
- **Completions like an editor.** A menu under the cursor at a zsh prompt: commands, flags, files, git branches, package.json scripts and make targets. ⇥ takes one. Grey suggestions from your history as you type, → takes them; ⌃R searches history.
- **Ask for a command.** Type `# what you want` at the prompt and press ⏎: Deck writes the command and puts it at the prompt for you to read and run.
- **Comes back with its scrollback** after a restart. JetBrains Mono and Nerd Font symbols are built in.
- **Your choice of look.** Font, size, line height, cursor, and six colour schemes, or one that matches the window.

### Browser

- **A real browser beside your code.** Search or type an address, with suggestions from your history and your search engine (Google by default).
- **A new tab page** with the sites you visit most and the dev servers running in your terminals.
- **Find in page, downloads, Web Inspector** (docked inside its pane), two-finger swipe back and forward, ⌘-click to open in a new tab.
- **Sign-in popups work.** Google, GitHub, Firebase and other popup sign-ins open in a real popup window that can talk back to the page. Alerts, confirms, site logins and camera prompts appear as they do in Safari.
- **Pages follow your theme,** in dark or light.

### Knows when an agent needs you

- **A "Needs you" badge** on the agent's tab and a count on the Dock icon when it asks for permission or a question.
- **macOS notifications** when you're looking elsewhere, for questions and for long jobs that finish. Click one to jump straight to that agent.
- **Live status.** Each agent's icon turns while it works, and its row says what it's doing ("Running cargo test", "Editing src/main.rs").

### A workspace that stays out of the way

- **⌘K command palette:** jump to any tab, action, snippet, file or recent page from one box.
- **Snippets** for the messages you send again and again (type `;review` and a space in a chat to put one in; a project can share its own in `.deck/snippets.json`) ("review my changes", "run the tests and fix what fails"). ⌘; lists them, and the one you pick goes into the chat composer or the terminal you are in, ready to add to and send. Save as Snippet keeps what you've typed, or a terminal selection; `{clipboard}` in a snippet puts in what you copied. Edit them in Settings › Snippets.
- **⌘N new tab page:** a terminal, a browser, or any agent, as a chat or in its own terminal.
- **Splits** you can drag to resize, with the focused pane clearly marked.
- **A sidebar by project.** Each project's chats, terminals and pages in one tree, in the order you opened them, with working agents in their own colour.
- **A status bar** along the bottom: the branch and project, commands running in any tab, open pages, and the dev servers on this Mac (only ones that answer HTTP). Click any of them to go there.
- **A code editor** for the project, with git built in: stage and commit, a history graph, worktrees and branches.
- **An inspector** for the focused pane: its processes, listening ports, project files and git status.
- **Comes back as you left it.** Tabs, splits and focus are restored after a restart, and agents in terminals resume their conversation.
- **Themes for the whole window:** System, Obsidian (true black), Black, Carbon, Graphite, Abyss, Midnight and Light, each with its own terminal colours.
- **Local models.** Settings › Models shows Ollama, LM Studio, llama.cpp and MLX, the models you have, and pulls more. Route Deck's own AI jobs (rewriting prompts, writing commands) to a local model; if it is not there, Claude does the job and the page says why.
- **Permissions in one place.** Settings › Permissions shows whether Deck has Full Disk Access, Accessibility and Screen Recording, asks for them where macOS allows, and opens the right System Settings pane for the rest.
- **Settings** you can search, applied as you change them.
- **Your default terminal,** if you like: shell scripts, `.command` files and `ssh:` links open in Deck.
- **Lock** (⇧⌘L) hides everything until Touch ID or your password; your terminals and agents keep running.
- **Updates** arrive by themselves and install on restart.
- **Right-click menus** everywhere, and one zoom (⌘+ / ⌘−) for everything.

## Getting started

Deck runs on macOS 13 or later.

```sh
git clone https://github.com/broisnischal/deck.git
cd deck
scripts/install.sh
```

This installs **Deck.app** into `/Applications` and the `deck` command into `~/.local/bin`. Open Deck from Launchpad or Spotlight.

To sell Deck, build it against an account server so it asks people to sign in and subscribe: `backend/` has one, with its own README.

Deck works with whichever agent CLIs you already have on your `PATH`: `claude`, `codex`, `gemini`, `grok`, `opencode`, `cursor-agent`, `amp`, `aider`. Run `deck doctor` to see which ones it found.

> For notifications from Claude Code running in a terminal, set `"preferredNotifChannel": "iterm2"` in `~/.claude/settings.json`. Deck understands that notification style.

### Windows

Needs 64-bit Windows 10 1809 or later (ConPTY and AF_UNIX sockets) and the Microsoft Edge WebView2 Runtime for pages. Windows 11 and current Windows 10 already have it; the installer offers the download when it's missing.

Run `Deck-<version>-setup.exe`. It installs for the current user only, no admin: `deck.exe` into `%LOCALAPPDATA%\Programs\Deck`, a Start menu entry, that folder on the user `PATH` (so `deck` works in new terminals), and `deck://` links. Uninstall from Settings › Apps. The portable zip is the same `deck.exe` without any of that.

Cross-build both from Linux:

```sh
cargo install cargo-xwin   # plus llvm (llvm-rc, lld), NSIS 3 (makensis) and zip
scripts/build-windows.sh   # dist/Deck-<version>-setup.exe and dist/Deck-<version>-windows-x64.zip
```

On Windows `deck.exe` runs the Alacritty engine; Ghostty's backend drives a Unix PTY, so it stays on macOS and Linux. To build natively, install Visual Studio Build Tools (MSVC and the Windows SDK) and Rust 1.96.1, then `cargo build --release`; no Zig needed. CI does exactly that and builds the installer (`.github/workflows/ci.yml`).

## Keyboard shortcuts

| Shortcut | Does |
| --- | --- |
| ⌘K | Command palette |
| ⌘; | Insert a snippet |
| ⇧⌘L | Lock Deck |
| ⌘N | New tab page |
| ⌘T | New terminal |
| ⇧⌘N | New chat |
| ⌘L | Browser address bar |
| ⇧⌘T | New browser tab |
| ⌘D / ⇧⌘D | Split right / down |
| ⌘W | Close pane |
| ⌘1 … ⌘9 | Go to tab (⌘9 is the last) |
| ⇧⌘] / ⇧⌘[ | Next / previous tab |
| ⌘] / ⌘[ | Next / previous pane |
| ⌘F, ⌘G / ⇧⌘G | Find, next / previous |
| ⇧⌘K | Clear terminal |
| ⌘B / ⌘I | Sidebar / inspector |
| ⌘+ / ⌘− / ⌘0 | Zoom in / out / actual size |
| ⌘, | Settings |

## Let agents drive Deck

Every terminal and chat in Deck can use the `deck` command, so an agent (or you) can work the window from a shell:

```sh
deck pane list                          # what's open right now
deck run --name dev npm run dev         # a server in a terminal beside you that you both watch
deck pane wait --name dev --for "ready|Local:"
deck pane read --name dev --lines 80    # the end of its logs
deck pane send --pane 5 'run the tests' # message another agent's chat

deck open --name app http://localhost:3000   # the page, in Deck's browser, beside you
deck browser snapshot --name app             # its headings and numbered elements
deck browser click --name app "text=Sign in"
deck browser type --name app 4 "me@example.com" --enter
deck browser errors --name app               # uncaught errors and failed requests
deck browser network --name app --failed
deck browser shot --name app page.png

deck task new 'add a dark mode toggle to settings'   # a sub-agent on its own branch
deck task list                                       # what every task is doing
deck task wait --task add-a-dark-mode                # until its agent is done
deck task diff --task add-a-dark-mode                # what it changed
deck task merge --task add-a-dark-mode               # squash it into your branch
```

Output is JSON. Run `deck` with no arguments for everything it can do.

Agents in Deck's chats also get all of this as tools of their own through MCP (`deck mcp`), including a look at the window itself. Deck also puts a skill in the skills folder of every coding agent you have (Claude Code, Codex, Gemini CLI, OpenCode, Cursor, Grok and others) that teaches it to run dev servers where you can watch them and to check pages through their console and network; turn it off in Settings › Agents.

## Releasing

```sh
scripts/release.sh             # dist/Deck-<version>.dmg and .zip, for Apple silicon and Intel
scripts/release.sh --publish   # and put them on GitHub as release v<version>
```

The disk image is what the website's download button gives people. The zip is what the in-app updater swaps in. Both find the newest release on their own, through the backend's `/v1/releases/latest` (and `https://deck.stroke.click/download/mac` always downloads the newest disk image).

Other Macs only open Deck when it is signed with a Developer ID certificate and notarized by Apple. That takes the Apple Developer Program and a one-time setup, described at the top of `scripts/release.sh`. Until it's done the script still builds, signs ad hoc for your own Mac, and refuses to publish. Raise `version` in `Cargo.toml` for each release.

## License

MIT
