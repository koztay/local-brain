# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

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
  daemon instead of the versioned Cellar path that `brew upgrade` deletes. Run `local-brain init`
  once in projects set up with 0.1.0.

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
