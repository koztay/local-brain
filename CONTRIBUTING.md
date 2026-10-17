# Contributing to local-brain

Thanks for your interest! Bug reports, ideas and pull requests are all welcome.

## Development setup

```sh
git clone https://github.com/koztay/local-brain
cd local-brain
cargo build
cargo test
```

You need Rust 1.80+ and a C compiler (tree-sitter grammars and SQLite are compiled from source).
A local model runtime is optional; every feature has a deterministic fallback, and the test suite
does not need a model.

To try your build against a project without touching your real setup:

```sh
export LOCAL_BRAIN_HOME=/tmp/lb-home
cd /path/to/some/project
/path/to/local-brain/target/debug/local-brain init --no-models
/path/to/local-brain/target/debug/local-brain learn --no-watch
/path/to/local-brain/target/debug/local-brain ask "where is main defined?"
```

## Code layout

| Path | Responsibility |
|---|---|
| `src/main.rs` | CLI definition (clap) and dispatch |
| `src/cli.rs` | `init`, `learn`, `status`, `doctor`, `uninstall`, `run`, … |
| `src/lang.rs` | tree-sitter extraction of symbols and edges per language |
| `src/learn.rs` | Index pipeline, chunking, import resolution, incremental re-indexing |
| `src/store.rs` | SQLite schema and queries |
| `src/query.rs` | `brain_query`: intent classification, structural/semantic/architectural answers |
| `src/summarize.rs` | Summaries, validation against the graph, architecture map |
| `src/slice.rs` | `brain_slice` and file outlines |
| `src/compress.rs`, `src/raw.rs` | Output compression and the raw store |
| `src/docs.rs` | Local image/PDF reading |
| `src/memory.rs` | Decisions, tasks, session summaries, `memory.md` sync |
| `src/hooks.rs` | Claude Code hook handlers |
| `src/mcp.rs` | MCP server (stdio JSON-RPC) |
| `src/integrate.rs` | Agent adapters (Claude Code; Codex planned) |
| `src/daemon.rs`, `src/registry.rs` | Watcher daemon and project registry |
| `src/runtime.rs`, `src/llm.rs`, `src/embed.rs` | Model runtime adapter, roles, embeddings |
| `src/hardware.rs`, `src/catalog.rs`, `models.toml` | Hardware profiling and the model catalog |
| `src/gen.rs` | Commit/PR messages, pre-review, documentation suggestions |
| `src/bench.rs` | Benchmark harness |

## Guidelines

* Keep hooks fast and fail-safe: a hook must never break the agent. Errors are logged, not raised.
* Every LLM feature needs a deterministic fallback.
* Never return whole files from `brain_query`; keep tool descriptions short (they cost tokens).
* Never drop error lines, stack traces or exit codes when compressing.
* Add tests next to the code (`#[cfg(test)] mod tests`). Run `cargo fmt` and `cargo clippy`.
* Adding a language: add the grammar crate, extend `Lang` and the `def`/`imports`/`callee`
  tables in `src/lang.rs`, add import resolution in `learn::resolve_imports`, and a test.

## Pull requests

1. Open an issue first for larger changes so we can agree on the approach.
2. Keep PRs focused; include tests and update the README/docs when behavior changes.
3. Use [Conventional Commits](https://www.conventionalcommits.org/) for commit messages
   (`local-brain commit` can draft them).

By contributing you agree that your contributions are licensed under the Apache License 2.0.
