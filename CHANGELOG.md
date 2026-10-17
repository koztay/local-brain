# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

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
