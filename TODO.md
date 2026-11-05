# deck — what a proper terminal app needs

Status: `[x]` built and verified · `[~]` built, not yet checked by hand · `[ ]` not built.
Each item says how it was verified.

## Terminal

- [x] Colours: 16 / 256 / truecolor, bold, italic, underline, dim, inverse (screenshots: zsh prompt, OpenCode)
- [x] Wide characters (CJK, emoji) and fallback glyphs stay on the grid (engine test `a中b😀c`; glyphs placed by column)
- [x] Cursor shapes from DECSCUSR (engine tests); block cursor with the glyph redrawn under it (screenshot)
- [~] Trackpad and wheel scrolling: pixel deltas now accumulate into lines (unit test: 30 × 4px = 6 lines; the old code gave 0). A real gesture can't be posted to a background window, so this needs a hand check.
- [~] Mouse reporting to TUIs (click, drag, wheel; alt-screen wheel → arrows); the encoding is engine-tested, not yet tried in a live TUI
- [~] Selection: drag, double-click word, triple-click line, ⌘C; Shift bypasses mouse reporting
- [~] ⌘-click a URL opens it in a browser pane beside the terminal (URL detection tested, including after wide chars)
- [x] Keyboard: ctrl/alt/function keys, application cursor mode (unit tests)
- [~] Text input through the IME (dead keys, accents, emoji picker, non-Latin) via the platform input handler
- [~] ⌘V paste with bracketed paste
- [~] ⌘K clears screen and scrollback (engine test for the clear)
- [x] Font zoom ⌘+ / ⌘− / ⌘0, saved across launches (driven through the View menu: font 13 → 15, settings.json written, screenshot)
- [x] UTF-8 locale for the shell (new pane prints `lang=en_US.UTF-8`)
- [x] New terminal opens in the focused terminal's folder (File › New Terminal Tab after `cd /private/tmp` gave `cwd=/private/tmp`)
- [~] Titles from the shell; bell marks the tab
- [~] Shell exit closes its pane

- [~] Two terminal engines, picked in Settings › Terminal › Engine for new terminals: Alacritty (`alacritty_terminal`, the default) and Ghostty (`libghostty-vt` 1.4, built with Zig 0.17; cargo feature `ghostty`, on by default). Both read through deck's scanner, so prompts, blocks, the shell's folder, notifications and dark/light answers are the same. Ghostty runs on its own thread (its handles are `!Send`) and holds frames during synchronized output. Ghostty still lacks: selection and copy, find, ⌘-click links that wrap, prompt jumps and block output, history restored after a relaunch (PTY, colours, wide chars, scrollback, resize, exit codes, colour queries and modes unit-tested; not yet used in the window)

## Workspace

- [x] Vertical tabs grouped Agents / Terminals / Browser; new, close, switch (⌘T, ⌘N, ⌘W, ⌘1–9, ⌃Tab)
- [~] Splits right / down, focus cycling (⌘[ ⌘])
- [~] Drag a split divider to resize (double-click evens the panes out)
- [~] Restore tabs and splits after a relaunch: `session.json` (saved within a second of any change) keeps every tab, its split tree, which pane had focus, the active tab, and whether the sidebar and inspector were open. Shells come back in their folder without replaying the command. An agent CLI in front comes back with its flags (never its prompt) and resumes: Claude Code by its conversation's id (matched per folder by recency, leaving out deck's own chats), Codex `resume --last`, OpenCode `--continue`. Pages reopen at their address, Settings reopens, chat threads take their saved place (flag filtering, resume lines, Claude's folder naming and the file format unit-tested)
- [~] Find in scrollback: ⌘F bar, Enter / ⌘G up to older output, ⇧Enter / ⇧⌘G down, wraps, smart case, query taken literally; matches in view highlighted, the current one in white (engine test: order, wrap, case, `1+1`, focus follows output into history, viewport-only highlight). Not yet tried in the window.
- [x] Sidebar (⌘B) and inspector (⌘I); the inspector folds itself away when the window is too narrow (screenshot at 690 px)
- [x] Inspector: processes, project dir, ports, files, git, all native except `git status` (timings: tree 0.04 ms, ports 0.01 ms)
- [x] Black monochrome design, real SVG icons throughout (screenshots)
- [~] ⌘N opens a new tab page: Terminal (preselected, so ⌘N ⏎ is a shell), Browser, and every agent deck knows with its mark, as a chat or in its own terminal (⌥⏎ or the card's terminal button); 1–9 pick, esc closes. ⌘T is a plain terminal, ⌘L the browser, ⇧⌘N a new chat. Only ⌘: ⌃L, ⌃N and ⌃T stay with the terminal.
- [~] Sidebar rows share one grid: the mark in a 16 pt slot at the row inset, every title on the column after it, two-line rows 44 tall, trailing buttons on one edge with the headers'; activity always shows in the mark's place (a terminal running an agent shows that agent's mark, turning while it works). Inspector: sentence-case section labels like the sidebar's, a 28 pt section switch matching the title bar buttons, the project as `~/path` with small reveal and copy actions, processes as an indented tree
- [~] Splits draw a card per pane, a gutter apart; the focused card has the brighter edge and the others dim (a page dims itself, since gpui cannot paint over it)
- [~] Tab titles: a program's own title (Claude Code's task, `vim file`), else the command in front; a second line says which project (git root) and folder the pane is in, or which site a page is on; threads say "Claude · project"
- [~] Agents that need you: a "Needs you" pill with a bell on the tab, a count on the Dock icon, and a macOS notification when you are looking elsewhere (clicking it opens that pane). Deck chats say what they wait on (the command to run, the question asked). Terminals: deck reads the notification escapes alacritty drops, OSC 9 (iTerm2) and OSC 777 (Ghostty), through a tap on the PTY reader that costs no throughput (bench: 146–160 MB/s), and Claude Code posts them once `preferredNotifChannel` is `iterm2`. A shell's own bell marks the tab without a banner. Looking at the pane clears it (escape parsing unit-tested; banners not yet seen)
- [~] Busy is what the program says: OSC 9;4 progress when it reports it, else Claude Code's title (`◐`/`◑` while working, `✳` at rest); only programs that say neither fall back to "printing in the last 1.5 s". Fixes the spinner on an idle Claude Code, whose status line keeps redrawing
- [~] Spinners: a comet for a busy terminal or a loading page, the agent's own mark turning while it works, and a light sweep across "Working…"
- [~] Right-click menus in deck's own look: terminal (links, copy, paste, select all, find, clear, splits, close), text fields, chat messages, sidebar rows
- [~] Terminal ⌘V pastes copied Finder files as paths and a copied image as a saved file's path, and files dropped on a terminal type their paths, so Claude Code and Codex attach them (path escaping unit-tested)
- [~] Window themes: System (follows macOS), Black, Graphite, Midnight and Light, chosen in Settings and applied at once to every surface, the menus, the terminal palette ("Match window") and the appearance native views take (traffic lights, scrollbars, web pages). Colours are read from the theme as the UI draws (409 reads converted), and every theme keeps text 12:1, secondary 4.5:1 and labels 3.9:1 or better on its surfaces (contrast unit-tested)
- [~] Settings laid out as System Settings: a sidebar of pages (General, Appearance, Terminal, Browser, Agents), one page at a time with a title, inset groups with a name on the leading edge and the control on the trailing one, hairlines that start where the text does, notes under groups, lists of choices with a check, the theme as preview tiles drawn in each theme's colours
- [~] Snippets: saved messages in `snippets.json` beside the settings. ⌘; (Edit › Insert Snippet…) lists them; the one picked goes in at the composer's caret or into the terminal as a bracketed paste, so nothing is sent or run until you press ⏎; elsewhere it goes on the clipboard. ⌘K finds them by name too. Save as Snippet keeps the composer's text or the terminal selection, named after its first line; typing in the ⌘; box offers to keep that text. `{clipboard}` expands on insert. Settings › Snippets lists, edits (saved per keystroke, ⏎ is a newline there) and deletes (two clicks). Four starters on first run (naming and expansion unit-tested; not yet seen in the window)
- [~] Usage, at the foot of the sidebar above Settings: shut, the row shows how full the focused conversation's context is (`ctx`, a small bar and a percentage), else the plan's 5-hour window, as the status bar's meter reads them. A click opens the same usage panel as the meter, hanging over the sidebar: the plan limits, the conversation's context, tokens and cost, today, and Codex. A chat whose agent has no transcript deck reads (OpenCode, Codex, Gemini) shows its own reports there, with the agent and model ("Claude · Opus 4.1"), and with more than one chat open the panel lists them by what they spent (click one to go to it). The row keeps the data fresh with the status bar off. Agents' reports now carry the context: Claude Code's message usage (context) and result (`modelUsage.contextWindow`), OpenCode's step tokens. Fixes two old overcounts: a turn's tokens were counted from both the streamed message and the result, and Claude's `total_cost_usd`, a running total for the process, was summed turn on turn (decoder and folding unit-tested; not yet seen in the window)
- [~] Keycaps everywhere a shortcut shows (`kbd.rs`, the ⌘K footer's look: 18 pt pill, 4 pt corners, faint tint, secondary text, UI face): the chat's hint line and its approval and question cards, the sidebar's New chat and Settings (and the empty sidebar's rows), as Settings › Shortcuts binds them, right-click menus (a word there, "chat" or "not installed", stays a word), the palette's footer, the new tab page. The palette's rows keep their quiet theme-matched hint. A hint line's parts are spaced instead of dotted. Keys written in a message (`⌃O`, `⇧⌘N`, `⏎`, `esc`) get the cap's tint and colour with a narrow no-break space each side, square since a text run can't round; not in code or link text, and `→` stays an arrow (detection and markdown marking unit-tested; not yet seen in the window)
- [~] ⌘K palette: the scrim takes the wheel and hover, so the list scrolls instead of the pane behind it; roomier rows (40 pt, icon tiles), header and keycap footer
- [~] Composer: the access chip is a chip like the agent's (mark, name, chevron), with no outline; it opens a menu of Ask first, Accept edits and Full access, each with what it allows and a check on the current one (↑↓ ⏎ esc, or click). It used to cycle on click, so reaching a mode restarted the agent at every step and one click from Accept edits was Full access. Ask first now survives a relaunch (it came back as Accept edits). Full access keeps full-strength text; stop is a solid square (menu dismissal checked against gpui's dispatch; not yet seen in the window)
- [~] Attachments: images show as square thumbnails cropped from their centre (`thumbs/` beside the threads), so a wide screenshot no longer spills across the composer: gpui's `ObjectFit::Cover` paints the image larger than its tile and nothing clipped it. The composer's × sits on the thumbnail's corner, inverted like the send button (it was the theme's text on fixed black, about 1.2:1 in Light). A click on any attachment, in the composer or a sent message, opens it inside deck: images at full size, text files with line numbers (first 3,000 lines / 256 KB), folders' entries, anything else with Open and Show in Finder; esc, space or a click outside closes it (cropping, text detection and loading unit-tested; not yet seen in the window)
- [~] Settings tab (⌘,, Deck › Settings…, or the gear at the foot of the sidebar), one per window, applied live and saved to settings.json: zoom; terminal colour scheme (Deck, Tomorrow Night, Catppuccin Mocha, Tokyo Night, Gruvbox Dark, Solarized Dark, One Dark); font family (installed monospace faces only), size, line height; default cursor (block / bar / underline), blink, Option as Meta; search engine (Google by default); the agent and access new chats start with. An old settings.json still loads, and a bad value resets only itself (serde round-trip, legacy file, cursor default and Meta keys unit-tested). Not yet seen in the window.

## Browser

- [x] Toolbar: back, forward, reload / stop, address bar (URL or search), close (screenshot)
- [~] Back / forward dim when there is nowhere to go; two-finger swipe goes back and forward (WebKit's own gesture)
- [~] Real loading progress along the toolbar edge, from WebKit's estimate, polled only while a load runs; a failed load no longer leaves the bar spinning forever
- [~] A pane's first page shows 250 ms after it commits (was: up to 1.2 s, or until every subresource finished, on every address-bar navigation); later loads keep the old page up until the new one paints
- [~] Safari's user agent, so sites send their current code instead of fallback bundles
- [~] Search with Google by default (or the engine in Settings); `?text` forces a search; localhost, `*.localhost` and LAN IPs open over http (unit-tested)
- [~] Address bar: first click selects the whole address; inline completion from history (Safari-style host, then full address; ⇥ accepts, ⌫ drops it); suggestions from history, dev servers and the search engine, ↑↓ ⏎ esc; the page steps aside while suggestions show (ranking and completion unit-tested)
- [~] History kept in `browser-history.json`, frecency-ranked, search result pages left out (unit-tested)
- [~] Site icons fetched natively (ICO, PNG, SVG, WebP), kept per host under `favicons/`, shown in the address bar, the sidebar and the new tab page
- [~] New tab page: frequently visited sites as tiles, dev servers running in your terminals
- [~] Find in page ⌘F (⌘G / ⇧⌘G, ↑↓, esc) with a match count
- [~] Downloads go to ~/Downloads with a toolbar chip; click shows the file in Finder
- [~] ⌘-click or middle-click a link opens it in a new tab; pages report single-page-app navigation, and deck reads the real address from WebKit rather than trusting the page (message parsing unit-tested)
- [~] Links that open new windows (target=_blank, window.open) become tabs
- [~] Address bar looks focused only while editing (was verified by screenshot before the redesign)
- [~] Web Inspector: right-click › Inspect Element, or ⌥⌘I
- [~] Pages render in dark appearance, so sites that follow `prefers-color-scheme` match deck
- [~] Page zoom ⌘+ / ⌘− / ⌘0 when a page has focus
- [~] Rounded bottom corners that match the card

## Agents and control plane

- [x] Master chat with Claude / OpenCode / Codex / Gemini / Grok: streaming text, tool cards, thinking, interrupt, agent switch mid-thread, Ask first / Accept edits / Full access
- [x] Agents drive deck through the `deck` CLI (Claude ran `deck pane list` from a chat and answered correctly)
- [x] Chat threads in the sidebar with live status
- [x] `deck pane send` / `pane read` work on chats, so one agent can message another
- [~] Agent TUIs as tabs with working / idle / needs-you status
- [x] `deck` CLI: pane, tab, browser, open; automation token (used for all the checks above)
- [x] Control chat pinned first and opened at launch (⌘1)
- [~] The control chat closes like any thread (× on hover, ⌘W; it is archived) and its row stays to start a fresh one; right-click it, or ⌘K, for New Conversation and Restart Agent (the same conversation on a fresh agent process)
- [x] Thread view: centred column, work logs that fold to "Worked for 4s · Ran 2 commands" (screenshot), Copy on hover, diffs struck and highlighted
- [x] Composer card with agent picker, Auto-accept edits / Full access, tokens and cost, round send / stop (screenshot)
- [~] Messages sent mid-turn go straight to Claude Code, which queues them itself and reads them as soon as the current tool calls finish, inside the same turn; deck shows them as "Queued · after this step" and moves each into the thread where Claude read it (`--replay-user-messages`), so the rest of the turn reads as its answer. ⌘⏎ sends with `priority: "now"`. Esc stops the turn and the queue goes out next; a message Claude drops comes back to the composer. Agents that can't take input mid-turn keep deck's own queue (protocol checked against the real CLI: mid-turn delivery, and the queue surviving an interrupt; decoder unit-tested)
- [~] Inbox sidebar: threads newest first, two-line rows with Working timer / Needs you / Done / time ago
- [~] Attachments: ⌘V pastes screenshots and copied images (saved under `attachments/`, scaled to 1568 px) and files copied in Finder; drag files in; + opens a picker. Thumbnails and chips in the composer and in sent messages (click opens). Claude gets images inline as base64, other agents a path. File-URL decoding unit-tested; a thumbnail seen in a sent message (screenshot).
- [~] `@` file mentions: git's file list (or a bounded walk), ranked by name match; ↑↓ choose, ⏎/⇥ insert, esc closes (ranking unit-tested)
- [~] Composer editing: ⌘Z / ⇧⌘Z (typing and deleting undo in runs; ⌘Z after send brings the message back), ↑ / ↓ walk sent messages, ↑↓ follow soft-wrapped lines, double-click word, triple-click line, caret kept in view when the text scrolls, a click anywhere on the card focuses it; `deck pane send` no longer wipes the draft (word/line selection unit-tested)
- [~] Threads persist: transcript, draft, attachments, ↑ history and turn times saved per thread (`threads/<id>.json`, debounced, flushed on quit); relaunch reopens the control chat and other threads; ⌘W archives to `threads/archive/`. Claude resumes its own session with `--resume`, falling back to replaying the thread if that fails; a session that ended is picked up with the thread instead of starting blank (restore seen in a test instance's log; round-trip unit-tested)
- [x] Markdown: inline code in the mono face, links clickable (open in deck's browser) including bare URLs, code blocks scroll sideways instead of wrapping with a language label and Copy, task lists, multi-line quotes and list items, `snake_case` no longer italicised, bordered tables (unit tests + screenshot)
- [~] Work log: steps read as sentences ("Ran `cargo test`", "Edited src/chat.rs", "Searched fn main"), present tense while running; running steps and logs spin, with a live seconds counter, and steps over a second say how long they took; the header counts "Working for 12s"; logs and opened steps fade in; commands open as `$ command` plus output (failed ones outlined); the newest plan (TodoWrite) shows as a checklist; edits show +/− and a diff built from their input (wording, plans, changed files and durations unit-tested)
- [~] "N files changed" card after each turn: every edited file with its +/−, a click shows all its changes
- [~] Approvals and agent questions docked above the composer: Claude runs with `--permission-prompt-tool stdio`, so a tool call that needs approval shows a card with the command or the diff and Claude Code's three answers (Yes · Yes, don't ask again in this chat · No), picked with ↑↓⏎, 1–3, a click, or esc; typing a reply refuses and tells Claude what to do instead. "Always" rules are kept to the session, never written to the project's settings. AskUserQuestion shows its options (multi-select too) and takes a typed answer; ExitPlanMode shows the plan. The step waits with "waiting for you", the thread shows Needs you, and the mode chip cycles Ask before edits → Auto-accept edits → Full access. (Format taken from the real CLI; round trip tested against a stand-in CLI; decoder unit-tested.) With claude-sudo armed, its hook approves everything first, so no card appears.
- [~] Select and copy text inside a reply: see the queue below
- [x] Prune pasted images no thread refers to any more (at startup, a week old and unreferenced; `threads::prune_attachments`)
- [x] Steer a running turn instead of queueing: see above
- [~] deck as an MCP server in every agent: registered once with Claude Code (user scope), Codex and Gemini CLI through their own `mcp add` (marker per agent under `mcp-registered/`)
- [~] Per-turn checkpoints with "Edit from here" (built earlier); worktree per thread: "Own branch" on a new thread's screen makes its first message start a task (its own branch and worktree, the same agent) in place of the empty thread. Diff panel: the task review; a thread-level diff panel not yet
- [~] Command palette ⌘K: open tabs, actions (with their shortcuts), the project's files (git's list) and visited pages in one box, fuzzy-matched with the matched letters lit, word starts and runs ranked first (matcher unit-tested); ↑↓ ⏎ esc; pages step aside while it is open. Typing a brief offers "Start a task". Clear terminal moved to ⇧⌘K
- [~] Prompt rewriting in the composer (from tower): ⏎ rewrites with a quick `claude -p` run (Haiku by default; no tools, no settings, no saved session, deck's and Claude's session variables stripped) using the thread's task and the agent's last reply, then sends; ⌃O rewrites into the box; ⌃Z brings your words back; short replies, `/` and `!` commands go as typed; a failed rewrite says so and ⏎ sends as typed; a message edited during the rewrite is left alone. Settings: Rewrite and send / Show first / Off, and Haiku / Sonnet / Opus, or a local model in Settings › Models. Off, nothing rewrites: not ⏎, not ⌃O, and a rewrite still running when it is turned off is dropped (⏎ then sends as typed) (flags checked against the installed CLI; rules unit-tested)
- [~] Auto-retry (from tower): a chat turn that dies on an API or network error gets "continue" after 5 s, 10 s, 20 s… up to 5 minutes, 8 tries, with a countdown under the composer (esc cancels, anything you send takes over); a usage limit waits for its reset ("resets 3pm" or the epoch Claude Code gives); login, billing and bad-request errors are shown instead. Claude Code in a terminal sitting in its own "Retrying in 18s · attempt 4/10" wait, or with no response, gets one Esc and, 5 s later, Enter if its message came back into the box or "continue" (classification, backoff and screen reading unit-tested)
- [~] Sub-agents: any agent in deck can hand out tasks with `deck task new 'brief'` (in the project it works in; `--agent`, `--cwd`), follow them (`task list`, `show` with the agent's last reply, `wait`, `diff`), steer them (`task send`) and finish them (`task merge`, `pr`, `discard`). The chat that handed a task out gets a message when its agent finishes a turn, with what it changed and how to review it; chat agents are told about all of this in deck's harness prompt (CLI parsing unit-tested)
- [~] Palette: the selection scrolls into view with ↑↓; hover and selection show on the palette's own surface
- [~] Tasks: "Start a task" makes a branch `deck/<slug>` and a git worktree under deck's data folder from what is checked out, and starts a chat with the default agent in it with the brief as its first message (Claude Code in a terminal if no chat agent is ready). The sidebar lists open tasks above threads: what the agent is doing or waiting on, else files and +/− changed (counted every 3 s through a scratch index, so new and deleted files count and the worktree's index is untouched). Review opens beside the work: the diff file by file, collapsible, Merge into the base branch (commits what is left, squashes into one commit named for the task, then removes the worktree and branch; refuses if the main checkout is on another branch), Open PR (push + `gh pr create`, the PR opens beside), Discard (asks twice), a terminal in the worktree. Kept in tasks.json (create → change → count → merge tested against a real repository)

## Terminal fixes

- [~] A click whose release happened outside the pane (over the sidebar, another pane, the menu bar) stayed "held" for the program, so every later move was reported as a drag and Claude Code's fullscreen mode selected as you scrolled, until the whole screen was grey. Releases outside the pane now reach it, and a move with no button down closes any press still open
- [~] Tab titles and agent detection use the terminal's foreground process group (and the CLI under `sh -c` for agent tabs), not the last process in the tree, which for Claude Code was one of its MCP helpers; a binary named after its version (`2.1.300`) is shown by the name it was started as (argv parsing unit-tested)

- [x] Selection no longer stretches over the screen when a full-screen app scrolls its own region (engine test reproduces the stretch)

## Performance

- [~] The sidebar and inspector are their own cached views: gpui redraws every ancestor of a view that changed, so before this each frame of a streaming agent in a terminal rebuilt the whole sidebar and inspector too (emulator core measured separately: 143 MB/s, 0.28 ms to snapshot a full coloured 200×60 screen)

- [x] Idle CPU 0.1% (was 3.6%): no per-second process spawning, and the sidebar repaints only when a status changes
- [x] Output redraws on PTY wakeup, not on a poll
- [x] Memory: 103 MB for the app with a terminal and an agent chat (debug build); each open web page adds WebKit's own process (~20–90 MB)

## Queue (2026-10-15), in order

1. [x] `deck window shot [PATH]`: a PNG of deck's own window, with pages in it, at full Retina size and without Screen Recording permission (an app may capture its own windows); 3024×1898 shot taken and checked
2. [~] Select and copy text in chats: drag across paragraphs, list items, table cells, code blocks and your own messages, even past the end of a reply into the next; double-click a word, triple-click a paragraph; ⌘C copies it as Markdown would write it (list markers, `>` quotes, tabs between cells); a click elsewhere clears it (selection, joining and run splitting unit-tested)
3. [~] Checkpoints: each message you send in a git project snapshots the working tree in the background as a commit under `refs/deck/checkpoints/<thread>/<n>`, through a scratch index (your branch, index, stash and ignored files untouched). Right-click your message: Edit from Here (files and chat back to before it, its text back in the composer) or Undo to Here; the session restarts with the thread so far replayed; Put Files Back undoes the restore. Refs go when the thread is closed (save, restore, staged changes kept, folders made by the turn removed, redo, forget: tested on a real repo)
4. [~] Preview per task: Preview in the review runs the project's dev command in the task's worktree (package.json dev/start/serve with its package manager, installing first in a fresh worktree; bin/dev; Django; Rails) on a free port of its own, and opens the page beside it once a process under that terminal listens (command detection unit-tested)
5. [~] `deck mcp`: deck's control socket as a stdio MCP server (17 tools: panes, split, new tab, read, send, focus, close, open, eval, the task commands, and a screenshot that returns the window as an image scaled to 1600 px). Every chat's Claude session gets it with that chat's token and `mcp__deck` allowed, like `Bash(deck:*)` (protocol unit-tested; initialize, tools/list, a real deck_panes call and a real screenshot driven over stdio against the running deck)
6. [~] Snippets v2: `;keyword` and a space in the composer expands a snippet (⌘Z brings the keyword back); project snippets from `.deck/snippets.json` in the repository, marked "project"; `{clipboard}` `{selection}` `{branch}` `{folder}`; keyword field in Settings; starters have keywords (trigger, expansion, old-file loading unit-tested)
7. [x] Speed: ⌘K's file list kept per project and refreshed behind (stale-while-revalidate, 20 s); long threads already parse Markdown incrementally per message and draw only what is on screen
8. [x] Pasted images no thread (open or archived) refers to are deleted at startup once a week old (name scanning unit-tested)
9. [x] Retry decides on the result event's `api_error_status` (401/402/403/404/413/400 yours to fix; 408/429/5xx/529 retried), a usage limit still waits for its reset, unknown statuses fall back to the words (decoder and classifier tested)
10. [~] Title bar: an avatar (your initial, or a person mark) whose menu has your name, Settings ⌘,, Theme, and Lock ⇧⌘L in red (⌘L stays the address bar); Lock shows only a lock screen until Touch ID or the Mac's password (LocalAuthentication); an update pill (Update available → Downloading update → Restart to update). Updates: check a minute after launch and daily (Settings switch), from the backend or GitHub's latest release; download a zip, check it is Deck by bundle id, swap it in on restart from a script that waits for deck to quit (version compare and release parsing unit-tested)
11. [~] Settings redesigned: a search box that shows every matching setting from all pages, a tree of pages with their sections (chevrons fold them, a click jumps to the section), sections titled over a hairline, each row its name and what it does on the left and its control on the right. New: General › Account (avatar, plan, Manage Plan, Sign Out), Updates (version, check, download, restart, check automatically), Default terminal. No keymap editor yet
12. [~] Paid app. `backend/`: a Cloudflare Worker (Hono + Better Auth with Google only + the Dodo Payments plugin: checkout, customer portal, a Dodo customer per account) with its data in D1 (migrations generated by Better Auth's CLI, plus `desktop_code`); bearer sessions; a browser sign-in that hands a one-time code back to `deck://auth/callback`; `/v1/me` asks Dodo for an active subscription each time; `/v1/releases/latest`. The app (built with `DECK_API`): a sign-in screen instead of the workspace until signed in with an active plan, the token in the Keychain, the account checked every 6 hours with a week's offline grace, `deck://refresh` after paying. Builds without `DECK_API` ask for nothing. Deployed to https://deck.stroke.click (broisnees, D1 `deck`, migrations applied); Google and Dodo keys and the Dodo product are still placeholders (checked: type check, migrations on a local and the live D1, the desktop flow under `wrangler dev`, every route live; Google's and Dodo's own pages need real keys)
13. [~] Default terminal: Info.plist declares shell scripts and ssh:, telnet:, x-man-page: (and deck:) links; Launch Services makes Deck their handler, or Terminal again; offered once on first launch in a corner card and in Settings › General; an opened script or link becomes a new terminal tab (link parsing unit-tested)

## Also fixed

- [x] macOS asked for permissions again after every install: the app was signed ad-hoc, so its identity changed with each build. `scripts/install.sh` now makes a local signing certificate once (its own keychain under deck's data folder) and signs with it, so the designated requirement (`identifier … and certificate leaf = H"…"`) stays the same and macOS keeps what you allowed (checked: same leaf hash across two signings, `codesign --verify --strict` passes, the app runs)
- [x] PATH no longer grows by deck's folder with every nested deck terminal (it held it 7 times)
14. [~] Keyboard shortcuts in Settings › Keyboard: every rebindable shortcut (tabs, panes, window, browser, terminal, chat) as a keycap; click it and press the new keys, which a keystroke interceptor takes before any shortcut acts on them; ⌫ removes one, esc keeps it; ⌘, ⌃ or ⌥ required; a clash with another shortcut or with Deck's own keys (copy, quit, ⌘1–9…) is named under the row; reset one or all. Saved as `keys` in settings.json, applied at once (keymap and menus rebuilt), and ⌘K, the menus and the sidebar show your keys (display and the allowed rule unit-tested)
15. [~] Chat while working: the live work log shows the newest four steps under "N earlier steps"; empty "Thought" rows are gone; steps rise in as they arrive; streaming text comes out at a steady pace that speeds up with the backlog, with a pulsing caret; after three quiet seconds a "Thinking · 12s" line says it is still going
16. [~] Terminal: programs' coloured backgrounds (Claude Code's diffs) drawn as a tint of the pane, greys and Deck's own selection left alone; Settings › Terminal can turn it off (softening unit-tested on the colours measured off a screenshot)
17. [~] Website at https://deck.stroke.click, served by the same Worker as the account server (one origin, first-party sign-in cookies): Home (live replay of an agent at work, agents, four feature spotlights with drawings, everything else), Features (every feature: what it does and how to use it), Pricing (price from Dodo, billing FAQ), Download (steps and requirements), Docs (getting started, tasks, undo, snippets, shortcuts, the deck command, files, troubleshooting), Changelog, Account (Google sign-in, plan, Manage billing through Dodo's portal, sign out), Privacy Policy, Terms of Service (Nepal law), Refund Policy (14 days on the first payment, 7 on renewals), 404, sitemap and robots. Fonts self-hosted, no trackers. Built from `website/src` by `website/build.mjs`; checked at 1440 px in headless Chrome, every route live

18. [~] Real screenshots on the website: a throwaway deck (its own data folder, HOME and ZDOTDIR; run unbundled so Panespace can fullscreen just that window) restores seeded threads (Claude's dark mode switch, Codex's flaky test, Gemini's README) beside a demo project whose tests really run and whose page is really served, and captures itself with `deck window shot` from a script in its own shell. Home shows the window with its four parts pinned and numbered; Features shows a Codex chat, Settings › Keyboard and Settings › Appearance; the hero is the social card (`og.jpg`). WebP, 40–195 KB each
19. [~] Releases: `scripts/release.sh` builds a universal binary (Apple silicon and Intel), assembles Deck.app with its version from Cargo.toml, signs it with a Developer ID and the hardened runtime, notarizes and staples the app and then the disk image, and makes `Deck-<v>.dmg` (for people) and `Deck-<v>.zip` (for the updater) with checksums; `--publish` puts them on GitHub as `v<v>`, refusing without Developer ID, notarization, a clean tree and a pushed HEAD. Release builds sign in to https://deck.stroke.click (`DECK_API`, so the trial and plan apply). The backend's `/v1/releases/latest` now also returns the `dmg`; `/download/mac` always downloads the newest one; the site's download buttons use the disk image. Needs the Apple Developer Program for other Macs to open it
20. [~] Free trial: every account gets 14 days (`DECK_TRIAL_DAYS`) from sign-up, no card. `/v1/me` answers `plan: "Trial"`, `active` and `trial_ends` (unix seconds), even while payments are down; a paid plan clears it. The app shows "Free trial, N days left" in Settings › Account and in the avatar menu (which opens checkout), stops on the day it ends even offline, and the gate says "Your free trial has ended". Website: pricing, home, FAQ, docs, account page (trial status and end date), terms and refund policy say so (trial and expired answers checked against a local D1; day counting unit-tested)

## 2026-11-02: control plane, pages, background work

Built in one pass on `sidebar-sessions-usage`. `cargo test -p app` passes (250). Not yet seen in the window: it needs a relaunch onto the new build.

### Chat
- [~] One chip for agent and model. Its menu lists the agents (ones not set up are marked) and, beside them, the hovered agent's models with a check on the current one. Picking another agent's model switches agent and model in one restart. Replaces the two rows of pills
- [~] The thread's tokens and cost sit right of the hint line under the composer, not in the toolbar, where they pushed the send button out of a narrow composer
- [~] New thread: "Codex in 📁 folder ⌄" opens a folder picker. The thread and its agent move there before the first message, and the sidebar re-places it

### Status bar
- [~] Chats in this window as items: agent mark (turning while it works), title, cost (or tokens). A click goes to it. Busiest first, four at most
- [~] Background jobs and background pages as items after the chats: a terminal or site mark, the name, a state dot (running, failed, idle, loading)
- [~] The drawer above the status bar shows a job's live terminal (Restart, Open as tab, Stop, Hide) or a background page (Open as tab, Close, Hide)

### Background work
- [~] `deck job start [--name N] [--cwd DIR] CMD` (and `deck run --bg`) runs a terminal outside every tab. Starting the same name restarts it in place. The name is guessed from the command (`npm run dev` → dev). Also `job list | logs | wait | show | stop`
- [~] Jobs come back at launch, saved in session.json with name, command and folder. Settings › Terminal › Background jobs switches that off and lists them
- [~] `deck browser open --bg --name N URL` opens a page outside every tab. It is laid out at 1280×900 while hidden and shown only in the drawer
- [~] `deck read URL [--name] [--scroll N] [--max] [--json]` opens a background page (Deck's WebKit, Safari's user agent, the user's cookies), waits for it to load and render, and prints the page as Markdown.
  - Kept: main content, headings, lists, quotes, code, tables, absolute links. Left out: navs, footers, forms, hidden parts. Open shadow roots and slots are read.
  - `--scroll` loads more of an endless feed. `deck browser read` reads the page you are on.
  - Extraction checked on a live GitHub page (tables fixed after). Reddit and X not yet tried on the new build

### CLI and skill
- [~] `deck history [QUERY] [--limit N]`: saved threads, newest first, whether open, past the relaunch window or archived. `deck resume ID` (an id prefix) reopens one, taking it out of the archive if need be, or focuses it if it is open
- [~] `deck pane key --pane N KEY`: enter, escape, tab, arrows, home/end, page up/down, ctrl+X, alt+X, shift+tab
- [~] `deck usage`: plan limits, the conversation, today, other agents and every chat's spend, as JSON
- [~] `deck page open usage|packages`
- [~] The skill Deck puts in every agent's skills folder at launch now covers background jobs, `deck read`, background pages, history, resume and usage (CLI parsing, job names and key bytes unit-tested)

### Pages
- [~] Usage page (⌥⌘U, ⌘K, or "History" in the usage panel):
  - Cost or Tokens, over the past 24h, 7, 30 or 90 days.
  - The total, with each agent's sessions and share, and a daily bar chart stacked by agent with a hover readout.
  - Totals: processed, cached input, uncached input, output, cache savings. Breakdown by model or by day.
  - Claude Code is read from its transcripts (a per-model split added to the cached scans). Codex is read from its `token_count` events at OpenAI list prices, kept apart from Claude's table.
  - 30 days here: 1.2 s cold, 10 ms warm
- [~] Packages page (⌥⌘P):
  - The project's dependencies: npm (pnpm, yarn, bun or npm, by lockfile), Cargo (with Cargo.lock), Python (pyproject or requirements; uv, poetry or pip) and Go.
  - Wanted, installed, and latest from the registry (fetched 8 at a time).
  - Filters: All, Outdated, Dev. Update and Update all.
  - Registry search (npm and crates.io) with Docs and Add. Adding or updating runs as a background job and opens it in the drawer.
  - Manifest reading and version comparison unit-tested

### Browser
- [~] Import from another browser (Settings › Browser, or ⌘K "Import from Browser"): Chrome, Arc, Brave, Edge, Vivaldi, Chromium, Safari.
  - History (the 3,000 newest pages) and bookmarks merge into the address bar's history.
  - Chromium cookies are decrypted (the keychain's "… Safe Storage" key, PBKDF2 and AES-CBC through CommonCrypto, Chrome 130+'s domain-hash prefix stripped) and put in WebKit's default store, so sign-ins carry over.
  - Safari history and bookmarks need Full Disk Access, and the import says so.
  - Chrome history (496 pages) and bookmarks read on this Mac. The cookie import is not yet tried by hand, since it brings up a keychain prompt; the cipher round-trip is unit-tested

### Sidebar and window
- [~] A relaunch brings back only tabs and threads touched in the last 3 days; pinned rows and the control chat always come back. Older threads stay on disk for `deck history` and `deck resume`
- [~] "Done" is a green check and the word, with no grey pill. The trailing column (time, state, close) has one width, so times line up. A split's pane count is on the second line instead of a third item on the edge
- [~] A hairline over Usage and Settings, so the list's last row no longer runs under them
- [~] Title bar: `📁 project / title`

### Second pass (same day)
- [~] Background service `deckd` (`deck daemon`, a LaunchAgent `com.broisnischal.deckd`, kept alive and started at login). Jobs and terminal agents run in its PTYs, so quitting Deck leaves them running; jobs running when it stopped start again (after a restart of the Mac). Output kept in a 512 KB ring and `jobs/NAME.log`. Deck's job panes run `deck job attach NAME` (tmux-style: replay, then keys, output and size pass through), so closing a pane or quitting Deck never stops the job. Deck asks the service every 2 s and attaches new jobs, closes panes of gone ones. `deck job start|list|logs|wait --for/--exit|show|attach|restart|stop` talk to the service directly and work with Deck closed (checked end to end on an isolated service: start, wait for text, wait for exit returns the job's code, logs, attach replay, stop, quit; unit tests for the PTY run, log text and argument quoting)
- [~] Menu bar icon (the service): a terminal mark with how many run (and ! when one failed); its menu lists every job and agent with state and uptime, each with Show in Deck (`deck://job/NAME`), Restart / Start Again, Stop / Remove; Open Deck, Stop All, Quit Background Service
- [~] Notifications you can answer: approvals carry Approve, Always Allow, Deny and Reply… (what to do instead); questions carry Reply…; a click still opens the pane. Answered in the chat without coming to the window
- [~] Sounds (Settings › Agents › Sounds): Needs you, Finished, Job failed, each one of macOS's sounds or None, stepped with ‹ › and played as picked; Volume. Played with the notification
- [~] Model menu: tighter rows, the models panel lined up with the hovered agent, the current agent in its colour, eases in (130 ms) and slides as the pointer moves between agents
- [~] Status bar chats as one item (their marks stacked, how many, total spend, how many working); hovering opens the list above it (fades in, stays while the pointer moves onto it)
- [~] Usage panel as four tabs, Limits, This chat, Chats, Today, switched by hover or click; a sliding indicator (180 ms ease-out), the content cross-fades, one fixed height so it never jumps
- [~] Sidebar: New chat and Search one quiet style; "Recent ⌄ / Projects ⌄" is the header itself (a click switches), the + shows only on hover; days under Recent are a step quieter; a Packages row at the foot beside Usage and Settings
- [~] Terminal completion menu: a coloured badge per kind on every row, full-width highlight, terminal font, the selected item's description in the footer

### Third pass (same day)
- [~] Terminal tabs run in the background service (Settings › Terminal › Keep running, on by default): each tab's shell (and any agent in it) is a session there, the pane attaches; quitting Deck leaves them running and a relaunch reattaches every tab, keeping its control token so `deck` inside still answers (the CLI finds Deck through a stable `deck.sock`). Closing a tab ends its session; `exit` closes the tab. Process scans (project, agent, ports) look at the session's shell, not the attach client. Sessions the saved layout doesn't name (a crash) come back as tabs. The menu bar lists them as "Claude · folder" / "Terminal · folder"
- [~] Flicker: a failed poll of the service no longer reads as "no jobs" (it closed and reopened every job pane every 2 s)
- [~] Deck memory: `deck memory add [--global] | list [--all] | search | forget`, per project or everywhere, in `memory.json`. A SessionStart hook in Claude Code's settings (`deck hook session-start`, merged in, others' hooks kept) hands every session the Deck brief and this project's memories; Codex, Gemini CLI and OpenCode get a managed block in their global instructions (between markers, the rest of the file theirs). Put back at every launch; `deck context` prints what a session is told (hook merge and block replace unit-tested)
- [~] System-wide shortcuts from any app (Carbon hot keys, no Accessibility permission): ⌃⌥Space shows or hides Deck, ⌃⌥N a new chat, ⌃⌥T a new terminal; listed under "System-wide" in Settings › Keyboard and rebound there like the rest (key parsing unit-tested)
- [~] Terminal keys: ⌘⌫ deletes to the line's start (⌃U), ⌘← / ⌘→ start and end of line (⌃A / ⌃E), ⌘⌦ to the end (⌃K), ⌥⌫ the word before (⌃W) (unit-tested)
- [~] ⌘J in a terminal: "Describe a command" box over the prompt; the command is written into the prompt, nothing runs until you press ⏎ (rebindable)
- [~] Completion: only commands that would run (PATH, builtins, your aliases and functions read from your rc files); history typos ("clea", "cdlear") are gone; names that start with what you typed beat scattered letters, which only show when nothing starts that way. Badges only when kinds are mixed; the selected item's description in a panel beside the menu, wrapped, instead of a cut-off footer
- [~] Sidebar selection: ⌘-click picks rows, ⇧-click a range; "N selected · Clear · Close". ⌘K "Close Tabs Idle for a Day" (pinned, working, waiting and Settings stay)
- [~] Themes Glass (dark) and Frost (light): the window blurs what is behind it, the sidebar and status bar let it through, panes stay almost solid. Settings › Appearance › Match coding agents' themes (on): changing the theme sets Claude Code's to ANSI dark/light, which draws with Deck's terminal colours
- [~] Model menu drawn above everything (the composer showed through), long model lists scroll (nine rows), a note that repeats the agent's name dropped; access menu on the same recipe and layer
- [~] Status bar chats list: two-line rows that fit (40 tall, line heights set), concentric corners, the conversation you are in first and marked Current (a terminal agent's included, from its transcript)
- [~] Packages page: the project is a control (folder picker), search results grouped by registry with counts, Docs/Add on the row under the pointer, "In project" for ones already there, the search box no longer a box in a box
- [~] Layout audit applied: one radius and spacing scale (6 controls, 8 rows, 10 popovers; 28 controls, 32 rows), one popover recipe and edge, context menus and hover-driven swaps no longer animate, popovers 120 ms; sidebar rows one height with trailers on one edge; settings page padding, nav rows, segments and buttons; packages and usage page controls

### Fourth pass (same day)
- [~] Branch picker: the status bar's branch and the inspector's Git card open a filter over the repository's branches (newest first, current checked); ⏎ or a click runs `git switch`, git's reason shown if it won't. Inspector Git card: the branch a control, Publish when unpublished, quiet buttons, quieter section tabs
- [~] Sidebar: rows one line (34 tall, 14 pt titles), a trailing time and state mark (ring working, bell waiting, filled check done), an activity or project tag; headers 12.5 semibold with counts, the view switch in the same style; Search as a field-like bar with a new-chat button beside it; the foot a single icon row (Packages, Usage with context %, Settings) with working / waiting / done counts that jump to the next
- [~] Pages in the sidebar have their own marks (chart, package); Usage and Packages take keyboard focus so ⌘W, ⌘K… reach Deck there; pages and the editor hand Deck's shortcuts (the keymap, not ⌘C/V/X/A/Z) to Deck instead of keeping them
- [~] New chat: a failed agent start is a slim notice with Try again; Home for ~; starters on one column
- [~] Deck and the service drop inherited agent-session markers (CLAUDECODE, CLAUDE_CODE_*), which turned Claude Code's transcript saving off in terminals Deck started
- [~] Matching agents' themes sets Claude Code to dark / light (its ANSI themes painted grey slabs with Deck's palette)
- [~] Reference apps no longer named in comments

## Next

In rough order of value.

- [~] Agents' themes follow Deck's: Claude Code `theme` auto (follows the terminal's dark/light answer live; the -ansi ones drop syntax highlighting), Gemini CLI `ui.theme` "ANSI" / "ANSI Light", OpenCode `tui.json` `theme` "system" (keys from their docs); one key per file changed, the rest kept (unit-tested). Codex documents no theme setting
- [~] Settings › Keyboard: its own search (name, group or keys, "cmd-j" or "⌘J") and where each shortcut applies ("In a terminal", "From any app"…), clash notes kept
- [~] Settings › Agents › Deck memory: every note with its project and id, Forget on each, a field that adds one for every project
- [~] Remaining audit fixes: new-tab corner button concentric, ⌘K group headers on the rows' edge, inspector section padding and 28-tall rows
- [~] A background job that fails on its own (not stopped or restarted on purpose, not a tab) plays the "Job failed" sound and posts "NAME failed · exited N: last line"; clicking it opens the job in Deck
- [ ] Deck's own structured chats keep running when Deck quits (today shells, terminal agents and jobs do). Plan: the chat's agent process runs under the service with pipes; Deck's driver talks to it through a relay; output while Deck is closed is buffered and replayed into the transcript on relaunch instead of restarting the agent with `--resume`

### Check by hand first
- [ ] Relaunch onto this build and look at every item above: model menu placement, drawer heights, status bar crowding with many chats and jobs, Usage and Packages at narrow widths, light theme
- [ ] Cookie import from Chrome end to end: the keychain prompt, then a signed-in site (GitHub, Reddit) in a Deck page
- [ ] `deck read` on Reddit (new and old), X, a docs site and a signed-in page. Hidden WebKit pages throttle timers, so check late-rendering sites and whether `--scroll` is needed

### Background work that outlives the app
- [~] A small `deckd` launch agent that owns the PTYs. Quitting Deck leaves jobs, agents and shells running, and reopening reattaches with scrollback. After a restart of the Mac it starts them again and resumes each agent's conversation
- [~] `deck job wait --exit`: until the command ends, exiting with its code
- [~] `deck events [--for working,waiting,idle,opened,closed] [--once] [--json]` (polls the window's panes; status per pane added to `pane list`): a stream of pane status, command exits, panes opened and closed, and notices (a streaming socket method)
- [~] `deck pane prompt --pane N|--name NAME TEXT`: send to an agent, wait until it stops working, print its answer

### Knowing what terminal agents are doing
- [~] Rules per agent (Claude Code, Codex, Gemini; a generic y/n for the rest; unit-tested on screen text; screenshot fixtures still to do) matched against the screen, the title and OSC 9;4, to tell working, blocked on you, and idle. Screenshot fixtures as tests. They feed the sidebar's state and "Needs you"
- [~] `deck report working|blocked|done|error|clear "msg" [--progress N]` (OSC 6973;report, written to /dev/tty; blocked notifies; the row's state and tag follow) (an OSC escape), so any program or agent can set its row's state and message
- [~] A SessionStart hook installed with the skill (Deck memory's hook; tells the agent its pane) (Claude Code, Codex) that tells the agent its pane and records its conversation id for resuming
- [~] Last-turn diffs for terminal agents: a checkpoint (out of the branch's way) when one starts working; `deck pane diff --name N` and ⌘K "Last Turn's Changes" show what it changed since. Keep / revert per hunk not yet

### Usage
- [~] OpenCode and Gemini CLI on the Usage page over a range (today only so far)
- [~] Usage by project, and a CSV export
- [~] History of plan limits: each reading folds into the day's peak per window (`limits-history.json`, 120 days); Usage page shows the last 14 days per window, coloured by how close to the cap

### Browser
- [ ] Saved passwords: import them from Chromium's Login Data (same key), keep them in the Keychain, and offer them in login forms
- [~] Safari cookies (Cookies.binarycookies, needs Full Disk Access; parser unit-tested)
- [~] Offer the import once: a bar under the browser's toolbar until answered (Import / Not now)
- [~] Ad and tracker blocking: third-party loads from ~50 ad/tracking networks, a WebKit content rule list, Settings › Browser › Privacy

### Sidebar and design
- [ ] The rest of the redesign: project header rows, the Recent / Projects switch, task rows and the control row in the new look; one spacing scale across sidebar, chat and pages
- [~] Sidebar filter (⌥⌘F): rows by title, project and detail; ⏎ opens the first
- [~] Composer: `/command` (built-ins, your and the project's commands) and `$skill` completion (Claude gets `/skill`). The branch line under it is still to do
- [~] ⌥⌘J jumps to the next row that needs you (⌘J is the AI command box); ⇧⌘↩ zooms a pane to fill its tab

### Packages
- [~] Search PyPI and Go; Remove (with Confirm); what depends on what from Cargo.lock / package-lock / pnpm-lock; npm audit and cargo audit badges and an Issues filter

### Control plane
- [~] `deck do ACTION` / `deck do --list`: every ⌘K action from the CLI, behind Settings › Agents › Command line
- [~] Remote hosts over SSH: hosts from the user's SSH config (aliases, Include, details; never written) plus `deck remote add`. ⇧⌘H / ⌘K "Connect to Host…": a folder on the host, then per host Terminal or an agent: a click opens a Deck chat whose agent process runs on the host (`ssh -T host 'cd DIR && exec agent …'` in its login shell, Deck's socket and token left out, Claude's local MCP config skipped), ⌥-click the agent's own interface in a terminal there. Terminals go through the background service like any tab. `deck remote list|add|remove|open HOST [--dir] [--agent] [--chat]`. Command building and config parsing unit-tested; not yet tried against a live host. Not yet: `deck` commands from inside a remote session (needs the socket forwarded and a Linux build), Tailscale discovery

### Chat scrolling and experience
- [~] Auto-follow: the thread follows new output while you are at the bottom; scrolling up pauses it; sending or jumping resumes it (built; the state machine is not unit-tested yet)
- [~] "Jump to latest": while paused, a pill over the composer with how many new messages arrived; a click goes to the end and follows again
- [x] Earlier history at the top: not needed as written. A thread loads whole and the list only lays out what is on screen, so there is nothing to fetch at the top; revisit if threads get long enough to load lazily
- [ ] Smooth scrolling: trackpads already scroll by the pixel (gpui's list); easing a mouse wheel's line steps needs the list to take an animated offset, which gpui 0.2's list doesn't expose without a fork; left for that
- [ ] Chat feel audit: message width and spacing, day separators and timestamps, hover actions (copy, retry, fork, edit), unread and "new since last visit" marks, queued-message placement, streaming pace and caret, and empty/error states. Review against the agents' own clients and fix what feels cramped or jumpy
- [~] Keep your place: opening or folding a step or a log (or "N earlier steps") anchors the list at the top of what you are reading, so it grows downward instead of sliding the row from under the pointer; "Jump to latest" resumes following. Approval cards and the composer don't move the list (it keeps its top item). Not yet checked by hand
- [~] Sticky prompt: scrolled into a reply, a slim bar over the thread shows the message it answers; a click scrolls back to it

### Chat rendering
- [ ] Sandboxed HTML in chat: render fenced `html` and artifact blocks in an isolated frame with no network, scripts off unless the block opts in, a clear "HTML" badge, Open in tab and Copy source. Check the content security policy, sizing to content, and that a hostile block cannot reach Deck or the page's cookies
- [ ] Canvas rendering in chat: render canvases produced by HTML artifacts and chart blocks to a crisp Retina image, export as PNG, cap size and frames so a runaway animation cannot freeze a thread, and fall back to the source when rendering fails
- [ ] Mermaid diagrams: render `mermaid` fences (flowchart, sequence, state, class, ER, gantt, mindmap, timeline) lazily and cache the result, pan and zoom, follow the window theme, and show the source with a parse error when a diagram is invalid. Visual fixtures in both themes and a long diagram
- [ ] Charts: render line, bar, stacked bar, area, scatter and pie charts from a fenced spec or CSV/JSON with labelled axes, legend, hover readout, theme colours, resize and PNG export; never execute chart JavaScript. Fixtures for missing data, long labels and narrow panes
- [ ] Diagrams beyond Mermaid: Graphviz/DOT and D2 fences rendered offline with pan and zoom, source fallback, and the same lazy loading and cache as Mermaid. Check a large graph does not stall the thread
- [ ] Diagram and chart interaction: click to open full screen, copy as PNG or SVG, and keep the rendered result when the thread is reopened instead of re-rendering from scratch
- [ ] Rendering budget: parse and render only what is near the viewport, cap concurrent renders, keep a cache keyed by source and theme, and add a measured long-thread fixture (hundreds of messages with code, tables, HTML, Mermaid and charts) with a time and memory budget
- [ ] Rendering tests: fixtures for HTML, canvas, Mermaid, charts and their fallbacks; visual snapshots in light and dark; malformed and hostile input; and a check that a failed render never takes the rest of the reply with it

### Accounts, devices and sync
- [ ] Remote login: sign in to Deck with the account from another Mac, a browser or a paired device, so its open tabs, threads, jobs and dev servers can be seen and controlled from this window; an expired or revoked session drops to the local workspace without losing anything
- [ ] Device pairing: link a second Mac with a short code or QR (never by copying a token), name each device, and list them with the last time seen and what they are running
- [ ] Device management: rename a device, sign it out, revoke it, and sign out everywhere; a revoked device loses remote access at once but keeps its local workspace, and the account page shows the same list
- [ ] Saved configurations: named presets for the whole workspace (theme, terminal colours and font, keybindings, snippets, browser search engine, agent and model defaults, permissions) that can be saved, edited, applied, exported and imported, with a per-project override where it makes sense
- [ ] Choose what syncs: settings, appearance, keybindings, snippets and memory each have their own switch; terminals, files, cookies, threads and attachments stay local unless explicitly chosen. Show exactly what leaves the Mac before the first sync
- [ ] Sync on the account: back up those presets to the account server, restore them on a new Mac during sign-in, and keep working offline. Per-key merge with a clear conflict card when two devices changed the same setting since the last sync; never last-write-wins silently
- [ ] End-to-end encryption: derive a key from the account's recovery phrase, encrypt synced presets in the Keychain and on the server, and show the recovery phrase once with a way to rotate it; the server never sees readable settings
- [ ] Never sync secrets: agent API keys, account tokens, browser cookies and passwords, SSH keys and environment secrets are excluded by construction, and a test checks that a full sync bundle contains none of them
- [ ] Remote workspace presence: a device shows whether it is awake and what jobs and dev servers are running there, with Open in this window, Stop job and restart; handle a device going offline mid-command
- [ ] Snapshot and move: export a signed bundle of the chosen configuration (and optionally the current tabs and threads) and import it on another Mac, with a version and an app-version check and a dry-run preview before anything is applied
- [ ] Sync tests: two instances against a local account server — first sign-in, a change on each device, a conflict, offline edits that reconnect, a revoked device, a rotated recovery key, and an import that would downgrade settings
- [ ] Migration and limits: a schema version for the synced bundle, forward and backward migration of settings, a size limit with what counts, and a clear error when the account's storage is full
- [ ] Remote login audit: the account page and Settings list sign-ins by device, IP and time, with a "this wasn't me" action that revokes the session and starts a re-login
