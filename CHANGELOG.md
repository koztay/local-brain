# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed
- Running a second local-brain binary (a development build, another install) in a project
  re-pointed that project's hooks and MCP server at it whenever the integration looked outdated
  to it, so Claude Code sessions silently ran that binary. Only the binary a project is
  registered with refreshes its integration now; a binary that disappeared is still repaired by
  whichever one runs next. `doctor` also flags an MCP registration that points at another
  binary (it only checked that the file exists).

## [0.1.8] - 2026-10-18

### Changed
- The per-prompt note now answers the prompt's "where" directly instead of guessing. Symbols
  named in the prompt (`createSafeFetch`, `Parser.parse`, bare camelCase/snake_case words) are
  resolved in the code graph to their definition (kind, file:lines, signature) and call sites;
  files named in the prompt get their local description (images/PDFs) or summary and key
  symbols; architecture questions get the repository map. Only prompts that name nothing fall
  back to the ranked areas and files, now without multi-thousand-line plans and specs unless
  the prompt is about documents. Measured on a 1,000-file monorepo, the note shrank from ~550
  tokens of mostly irrelevant files (the right file third in the list) to ~120 tokens with the
  answer; definitions shorter than 14 lines are quoted whole, so "where is X and what does it
  do" needs no tool call at all. `prompt_context_tokens` defaults to 350.
- The session brief is a third of its size: the overview keeps its first sentences (without the
  small model's "Okay, here's an overview…" lead-in), at most four decisions and tasks, and no
  tool pitch (the CLAUDE.md section has it). `brief_tokens` defaults to 350.
- The CLAUDE.md section is shorter and tells Claude to trust the note, read code in ranges, and
  use Read for images and PDFs (local-brain replaces them with the local description) instead
  of `brain_doc`.
- Images are only replaced by their local description when that is cheaper than the image
  itself (a 200×200 fixture costs Claude ~50 tokens; its description more), and not when the
  prompt's note already carried the description — then a Read is a request for the pixels and
  returns them. The note gives an image's gist (300 characters) and a PDF's full text. The
  replacement's preamble is one line. Previously image tasks paid for the description and then
  for the image Claude fetched anyway; measured, a lone image is where a local model saves the
  least (Claude reads one for ~50–1,600 tokens), PDFs and batches of screenshots are where it
  pays.
- Architecture questions get the module map with one line per top-level module.
- The MCP server marks its tools always-loaded (`anthropic/alwaysLoad`), and `.mcp.json`
  registrations set `alwaysLoad`. Claude Code otherwise defers MCP tools and needs a
  `ToolSearch` turn before the first `brain_*` call — about 400 new tokens and a round trip
  each time, which cost more than `brain_doc` saved.

### Fixed
- `bench` reset each worktree with `git clean` between tasks, which deleted graphify's untracked
  index and CLAUDE.md after the first task: 17 of 18 "graphify" sessions were really vanilla,
  and the comparison in 0.1.7's reports was wrong. Each configuration's setup is now committed
  in its detached worktree, so resets keep it.
- `bench` gave local-brain sessions the 0.1.4 hook layout (Read in PreToolUse, no
  UserPromptSubmit) and an ad-hoc system-prompt line instead of the CLAUDE.md section. It now
  installs exactly what `init` installs, from the same code.
- `bench` ran one configuration after another; the later ones got a warmer prompt cache (about
  2.4k more cached tokens per session on the second configuration). Configurations are now
  interleaved per task; reports say which order was used.
- `bench` reports name the model the sessions actually ran on. Without `--model` that is Claude
  Code's default, which changes with `/model`; a run on a pricier default looked like a
  regression.
- `bench` sessions no longer write session summaries into the project's real memory (the memory
  hooks are left out; they cost no tokens).

## [0.1.7] - 2026-10-18

### Added
- `local-brain statusline`: shows in Claude Code's status line how many tokens local-brain
  saved in the current session and today, or what it cost when context notes outweighed the
  savings. ~10 ms per refresh; prints nothing outside a local-brain project. `--install` sets it
  as the status line only when none is configured, and otherwise prints the line to add to your
  own script.

### Changed
- `bench` reports rank configurations by cost and lead with **new tokens** (uncached input +
  cache writes, billed at full price). The old headline, total tokens, is dominated by cheap
  cache reads and showed savings that cost more. `bench --report <json>` re-renders a saved run.
- `bench` sessions leave out the user's own Claude Code settings and plugins
  and local settings (`--setting-sources project`); with plugins installed they added the same
  ~10k-token floor to every configuration and hid the differences. Local settings are left out
  because Claude Code reads them from the main checkout for a git worktree: every configuration
  inherited the project's own local-brain hooks, and the local-brain configuration got its brief
  twice plus the per-prompt note. `--with-user-settings` restores both.
- `bench --repeat N` runs the task set N times and shows each configuration's cost range, so
  run-to-run noise can be told apart from real differences.

### Fixed
- Session memory no longer records "decisions" and "todos" from sessions that changed no files.
  From Q&A sessions the local model restated its answer as a decision ("Used src/util.rs:144 to
  locate …"); these filled the session brief that every later session pays for, and changed it
  from one session to the next. Placeholder todos ("None", "N/A") are dropped. To clean an
  existing project: entries are archived, not deleted, with `local-brain memory done <id>`.
- The per-prompt context note was also added to prompts Claude Code generates itself
  (background-task and monitor notifications, local command output), about 450 tokens each
  that nobody asked for; in one long session these were most of the note's cost. They are now
  skipped; typed prompts, including pasted markup, still get the note.

## [0.1.6] - 2026-10-18

### Added
- `local-brain config` lists every setting with its value, default, meaning and tuning notes;
  `config set <section.key> <value>` / `config reset <key>` change one without editing TOML
  (type- and range-checked; API keys are never printed).
- `local-brain optimize` tunes the memory and background settings to the machine's RAM
  (defaults are for 16 GB), shows the changes and asks before writing. Models are not switched;
  a tier mismatch is pointed out with `init --reconfigure`.

### Fixed
- The background summary queue kept the local model loaded indefinitely (each request reset
  `keep_alive`), which on 16 GB machines pushed macOS into many GB of swap on the system disk.
  Background passes now back off under memory pressure and unload the model while waiting
  (`[daemon] min_free_memory_pct`, default 15; pacing in `bg_*` settings). The idle unload also
  works again after a background pass.

## [0.1.5] - 2026-10-17

### Security
- 0.1.4 read images/PDFs outside the project in a `PreToolUse` hook, which runs *before*
  Claude Code's permission check, so a file Claude Code would have refused could still be read
  and described to the model. Read is now handled entirely in `PostToolUse`: local-brain only
  processes files Claude Code was allowed to read, and deny rules apply. Paths are resolved
  (symlinks, `..`) before deciding whether a file is in the project. Please update from 0.1.4.

### Changed
- Large files are also replaced by their outline after the read (same savings). Escape hatch: a
  Read with `offset`/`limit` is never replaced — the original image with `offset: 1`, a large
  file in ranges — because Claude Code rejects an identical repeated Read.
- The image description notes that a small local model can misread exact text and numbers.
- Existing projects get the new hook configuration automatically on the next command (the
  check now compares matchers and timeouts, not only which events are present).

## [0.1.4] - 2026-10-17

### Added
- Per-prompt context (`UserPromptSubmit` hook): for task prompts ("improve the admin panel's
  look") local-brain adds a short note on where the code lives — matching areas of the tree with
  their sections, the most relevant code files with key symbols, then related docs — so Claude
  starts from the right files instead of exploring. Works for prompts in other languages too
  (directory names match word stems, e.g. "panelinin" → `panel/`). Skips confirmations and
  slash commands. `[hooks] prompt_context`, `prompt_context_tokens`.
- Images and PDFs outside the project (e.g. `~/Desktop/Screenshot.png`) are read locally as well
  when Claude opens them (`[hooks] read_outside_docs`). Images pasted into the prompt still go
  directly to Claude; no hook can intercept them.

### Changed
- The `CLAUDE.md` section asks Claude to use local-brain for every task, not only for questions.
- Existing projects get new hooks and the updated `CLAUDE.md` section automatically on the
  first command after an update.

### Fixed
- Session memory: sessions without a transcript (Claude Code writes none for a session without
  messages) no longer log an ingest error.

## [0.1.3] - 2026-10-17

### Changed
- `learn` shows each step as `[n/4]` with rate, ETA and duration, and prints "up to date" for
  steps with nothing to do. Off a terminal it prints one line per step instead of redrawing.
- `learn` no longer waits for model-written summaries when watching is on: it finishes with
  rule-based summaries and the daemon upgrades them in 30-second background passes, so file
  watching stays responsive. `status` shows how many are still pending. `--no-watch` keeps the
  old foreground behaviour.
- Model summaries go to the most referenced files first and stay within `max_llm_files` (other
  files keep rule-based summaries instead of being upgraded on every pass).
- Smaller prompts and answers on tier S (outline ≤ 400 tokens, source ≤ 600, answer ≤ 90):
  measured on gemma3:4b / Apple M4, ~5–7 s per file summary instead of ~10 s.

### Fixed
- The daemon's start-up catch-up pass only ran after the first file change; it now runs for
  every watched project.
- `status` no longer prints negative savings for session briefings ("adds ~N tokens of context").

## [0.1.2] - 2026-10-17

### Fixed
- `local-brain update` on Homebrew runs `brew update` first; `brew upgrade` alone does not
  refresh third-party taps, so it reported nothing to upgrade.
- `update` checks the version of the binary it installed instead of trusting the package
  manager's exit status.
- Projects whose Claude Code hooks or MCP entry point at a binary that no longer exists (for
  example the 0.1.0 Homebrew path removed by `brew upgrade`) are re-pointed automatically by the
  next interactive command or update; the daemon service is repaired the same way.

## [0.1.1] - 2026-10-17

### Added
- Update notifications: a daily background check of the latest GitHub release; interactive
  commands print a one-line notice and `init` offers to update, then continues with the new
  version. Off with `[network] check_updates = false` or `LOCAL_BRAIN_NO_UPDATE_CHECK=1`.
- `local-brain update [--force]`: upgrades via Homebrew, cargo, or by swapping in the
  checksum-verified (and, on macOS, signature-checked) release binary; restarts the daemon.
- Models can be stored on an external drive: `init` asks where when one is connected
  (`runtime.ollama_models_dir`).
- `init` installs, starts and configures Ollama when no local model runtime is found (Homebrew or
  the signed app on macOS, the official installer on Linux, winget on Windows), then downloads the
  tier's models — after a single confirmation, or automatically with `--yes`. An installed but
  stopped Ollama is started automatically.

### Fixed
- Homebrew installs register the stable `<prefix>/bin/local-brain` path for hooks, MCP and the
  daemon instead of the versioned Cellar path that `brew upgrade` deletes.

### Changed
- The release workflow updates the Homebrew formula automatically.

## [0.1.0] - 2026-10-17

### Added
- `init`: hardware profiling (RAM, Apple Silicon, NVIDIA, disk, runtimes), S/M/L model tiers from a
  versioned `models.toml` catalog, model download via Ollama, Claude Code integration (MCP server,
  hooks, `CLAUDE.md` section).
- `learn`: tree-sitter code graph for TypeScript/JavaScript, Python, Go, Rust, Swift and Kotlin
  (symbols, imports, calls, inheritance, import resolution); chunking; FTS5 + embeddings (local
  model or built-in hash embeddings); file/module/project summaries with graph validation;
  architecture map; local PDF/image reading; resumable, hash-based incremental runs.
- MCP server with `brain_query`, `brain_slice`, `brain_doc`, `brain_raw`, `brain_memory`.
- Claude Code hooks: local image/PDF reads, large-file outlines, Grep/Glob ranking and trimming,
  Bash output compression, session brief, session memory.
- Output compression with never-drop rules for errors and stack traces, secret masking and a raw
  store with TTL; `run` wrapper.
- Memory: decisions, open tasks, notes, session summaries; editable `memory.md`.
- Global daemon with debounced watching, instant AST updates, background refresh, idle model
  unloading; launchd/systemd/Task Scheduler service installation.
- Runtime adapter for Ollama, LM Studio, MLX servers, llama.cpp `llama-server` and other
  OpenAI-compatible servers, plus a deterministic no-model mode.
- `commit`, `pr`, `review`, `docs` delegated to the local model with deterministic fallbacks.
- `bench`: reproducible task sets and real headless Claude Code comparisons (vanilla, graphify,
  local-brain) with Markdown/JSON reports; `--offline` estimate.
- `status`, `doctor`, `uninstall`.
