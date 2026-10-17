# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

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
