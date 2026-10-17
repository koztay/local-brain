# local-brain

**A local helper brain for coding agents.** local-brain learns your project on your own machine —
code graph, embeddings, summaries, documents, decisions — so agents like Claude Code ask it
instead of reading the repository. The expensive cloud model spends its tokens on designing and
writing code; exploration, image/PDF reading and output digestion happen locally, for zero cloud
tokens.

[![CI](https://github.com/koztay/local-brain/actions/workflows/ci.yml/badge.svg)](https://github.com/koztay/local-brain/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

```
$ local-brain ask "who calls index_file?"
Call sites of `index_file` (2 sites in 1 files):
- src/learn.rs:491 in `learn`
- src/learn.rs:589 in `reindex_paths`
[confidence 0.90 · structural layer]
```

---

## Why

Coding agents burn most of their tokens *before* they write anything: grepping, opening files,
reading screenshots and PDFs, and scrolling through test and build logs. local-brain removes that
waste:

| Where tokens go | Without local-brain | With local-brain |
|---|---|---|
| "Where is X / who calls X / how does Y work?" | Grep + read whole files | `brain_query`: short answer + `file:line` pointers |
| Editing code | Read the whole file | `brain_slice`: just the symbol or line range |
| Images, PDFs, diagrams | Sent to the cloud model as image tokens | Read by a **local** vision model / PDF extractor |
| Test, build, lint, log output | Pasted into context in full | Compressed; errors and stack traces kept verbatim; full text behind a `raw_id` |
| Huge Grep/Glob results | Hundreds of lines | Ranked with the code graph, top results + `raw_id` |
| Big file reads | Thousands of lines | File outline first, then targeted slices |
| Each new session | Re-explores the repo | Short brief: architecture, recent decisions, open tasks |
| Commit/PR messages, doc stubs, pre-review | Cloud model | Local model (with deterministic fallbacks) |

Every call is metered: `local-brain status` shows how many tokens Claude would have seen versus
what it actually saw, and `local-brain bench` runs real Claude Code sessions to compare vanilla
Claude Code, Claude Code + graphify, and Claude Code + local-brain.

## How it works

```
┌──────────────────────────────── your machine ─────────────────────────────────┐
│                                                                               │
│  Claude Code ──MCP (stdio)──► local-brain mcp     5 tools: brain_query,       │
│      │                         │                  brain_slice, brain_doc,     │
│      └──hooks────────────────► local-brain hook   brain_raw, brain_memory     │
│                                │                                              │
│                                ▼                                              │
│               <project>/.local-brain/brain.db   (SQLite, WAL)                 │
│               code graph · chunks + FTS5 · embeddings · summaries ·           │
│               documents · memory · metrics          + raw/ outputs (TTL)      │
│                                ▲                                              │
│  local-brain daemon ───────────┘  watches all projects, re-indexes on save,   │
│      │                            refreshes embeddings/summaries when idle    │
│      └── runtime adapter ──► Ollama │ LM Studio │ MLX server │ llama-server   │
│                              (or none: deterministic mode)                    │
└───────────────────────────────────────────────────────────────────────────────┘
```

* **Code graph (deterministic, no LLM):** tree-sitter extracts symbols (functions, methods,
  classes, structs, traits, interfaces…), imports, calls and inheritance for **TypeScript/JavaScript,
  Python, Go, Rust, Swift and Kotlin**. Imports are resolved to project files. Other languages and
  prose are indexed for search and summaries.
* **Hybrid search:** SQLite FTS5 (BM25, Porter stemming) + vector similarity, fused and reranked
  by query-term coverage. Embeddings come from your local embedding model, or from a built-in
  hashing embedder when no model is installed.
* **Summaries:** file → module → project, bottom-up, each bound to the hash of its sources. Names
  in LLM summaries are checked against the graph; summaries that mention symbols that do not exist
  are regenerated, and flagged if still wrong (hallucination filter).
* **Architecture map:** entry points, modules with fan-in/fan-out, main dependencies and most
  referenced symbols, computed from the graph.
* **Tiered answers:** `brain_query` picks the cheapest layer that can answer — structural
  (graph), architectural (summaries) or semantic (search + optional local LLM answer) — and never
  returns whole files.

## Quick start

### 1. Install

local-brain is a single, self-contained binary (no Rust, Python or Node needed).

**macOS and Linux:**

```sh
curl -fsSL https://raw.githubusercontent.com/koztay/local-brain/main/install.sh | sh
```

The script downloads the binary for your platform from the
[latest release](https://github.com/koztay/local-brain/releases/latest), verifies its SHA-256
checksum and installs it to `~/.local/bin` (set `LOCAL_BRAIN_INSTALL_DIR` to change it, or
`LOCAL_BRAIN_VERSION=v0.1.0` to pin a version).

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/koztay/local-brain/main/install.ps1 | iex
```

Installs to `%LOCALAPPDATA%\local-brain\bin` and adds it to your user `PATH`.

**Homebrew (macOS, Linux):**

```sh
brew tap koztay/local-brain https://github.com/koztay/local-brain
brew install local-brain
```

**Manual download:** grab the archive for your platform from the
[releases page](https://github.com/koztay/local-brain/releases/latest), check it against
`SHA256SUMS`, unpack it and put `local-brain` on your `PATH`.

| Platform | Archive |
|---|---|
| macOS, Apple Silicon | `local-brain-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `local-brain-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 | `local-brain-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `local-brain-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `local-brain-x86_64-pc-windows-msvc.zip` |

> The macOS binaries are signed with a Developer ID certificate and notarized by Apple.

**From source** (needs [Rust](https://rustup.rs) 1.80+ and a C compiler):

```sh
cargo install --git https://github.com/koztay/local-brain --locked
```

Check the installation with `local-brain --version`. To upgrade, run the same install command
again.

### 2. (Optional, recommended) a local model runtime

local-brain works without any model (graph, search, compression and memory are deterministic).
A local runtime adds summaries, local image reading, smarter compression and generated
commit/PR texts. Any of these works; the first one found is used:

| Runtime | Setup |
|---|---|
| [Ollama](https://ollama.com) (easiest) | Install and start it. `local-brain init` offers to download the right models. |
| [LM Studio](https://lmstudio.ai) | Start the local server (port 1234) and load the models named in `~/.local-brain/config.toml`. |
| MLX (Apple Silicon) | `pip install mlx-lm` then `mlx_lm.server --port 8080` (`mlx_vlm.server` for vision). |
| llama.cpp | `llama-server -m model.gguf --port 8080` (add `--embeddings` for an embedding server). |

### 3. Set up a project

```sh
cd your-project
local-brain init      # profiles hardware, picks models, wires up Claude Code
local-brain learn     # builds the index (resumable), then watches for changes
```

Restart Claude Code in that folder. That's it: Claude Code now has the `brain_*` tools, the hooks
are active, and `CLAUDE.md` tells it to ask the brain first.

## Testing it on your machine

A checklist to verify everything end to end (≈10 minutes):

1. **Install and check:** `local-brain --version`, then in a real project run
   `local-brain init` and `local-brain learn`.
2. **Health check:** `local-brain doctor` — every line should be ✓ (the daemon line is optional).
   `local-brain doctor --deep` also runs a generation test against your model.
3. **Ask from the terminal:**
   ```sh
   local-brain ask "where is <SomeClass> defined?"
   local-brain ask "who calls <some_function>?"
   local-brain ask "give me an architecture overview"
   local-brain ask "how is authentication handled?"
   local-brain slice src/some/file.ts --symbol someFunction
   ```
4. **Compression:** `local-brain run -- npm test` (or `cargo test`, `pytest`, …). Long output is
   compressed; errors stay verbatim; `local-brain raw <raw_id> --grep error` shows the full text.
5. **Live updates:** `local-brain status` shows `Daemon running`. Add a function to a file, save,
   and `local-brain ask "where is <new_function>"` finds it within a second or two.
6. **Claude Code:** start `claude` in the project. Run `/mcp` — `local-brain` should be connected
   with 5 tools. Ask "Who calls X?" and watch it use `brain_query`. Ask it to run the tests and
   notice the compressed output. Ask it to read a large file and notice the outline + slice flow.
   Drop a screenshot or PDF in the repo and ask about it.
7. **Savings:** `local-brain status` shows per-source token savings for today and all time.
8. **Memory:** after a session, `local-brain memory show` (or open `.local-brain/memory.md`) shows
   the session summary, decisions and open tasks. Edit the file; your edits win.
9. **Benchmark:** `local-brain bench --offline` gives a free estimate;
   `local-brain bench --configs vanilla,local-brain --limit 5` runs real headless Claude Code
   sessions (uses your Claude plan/API credits) and writes a Markdown + JSON report.
10. **Clean up (optional):** `local-brain uninstall --project` removes everything from one project.

## Commands

| Command | What it does |
|---|---|
| `local-brain init [path]` | Creates `.local-brain/`, registers the project, profiles hardware (first run), selects/downloads models, installs the Claude Code integration (MCP server, hooks, `CLAUDE.md` section). `--reconfigure`, `--provider`, `--no-agent`, `--no-models`, `--yes`. |
| `local-brain learn` | Full incremental index: code graph, embeddings, summaries, documents, validation. Shows progress, resumes after interruption, then turns watching on. `--no-llm`, `--no-watch`. |
| `local-brain watch [on\|off]` | Add/remove the project from the background daemon. |
| `local-brain status` | Index state, model tier, runtime, daemon RAM, last update, token savings. |
| `local-brain ask "<question>"` | Ask the brain from the terminal (`--scope src/api`, `--no-llm`). |
| `local-brain slice <path> [-s symbol] [-l 10-40]` | Print just a symbol or a line range. |
| `local-brain doc <path> [-q question]` | Read an image or PDF locally. |
| `local-brain run -- <cmd>` | Run a command and print its output compressed; keeps the exit code. |
| `local-brain raw <raw_id> [--grep re] [--range a-b]` | The full text behind a compressed output. |
| `local-brain bench` | Benchmark vanilla / graphify / local-brain with real Claude Code sessions (`--offline` for an estimate). |
| `local-brain doctor` | Check runtime, models, database, hooks and MCP. `--set-provider ollama\|lmstudio\|mlx\|llamacpp\|openai\|none [--url …]`, `--deep`. |
| `local-brain commit` | Conventional Commits message for staged changes. `--commit` to commit (opens your editor), `--install-hook` for a `prepare-commit-msg` hook. |
| `local-brain pr [--base main]` | Pull request title and description from the branch diff and the decision log. |
| `local-brain review [--base main]` | Pre-review: possible secrets, debug statements, conflict markers, removed symbols still in use, unused new code, plus an optional local-model pass. `--install-hook` adds a non-blocking git pre-commit hook. |
| `local-brain docs [path]` | Docstring and CHANGELOG suggestions. Never writes files. |
| `local-brain memory show\|note\|done\|path` | Project memory: decisions (what + why), open tasks, notes, session summaries. |
| `local-brain daemon run\|start\|stop\|status\|install\|uninstall` | The global daemon; `install` registers a launchd agent (macOS), systemd user service (Linux) or a logon task (Windows). |
| `local-brain uninstall [--project]` | Remove the integration and index from one project, or from all projects plus global data. |
| `local-brain mcp` | The MCP server (Claude Code starts it; you normally don't). |

## Claude Code integration

`local-brain init` makes three changes, all reversible with `local-brain uninstall --project`:

1. **MCP server** registered with `claude mcp add --scope local local-brain -- <binary> mcp`
   (stored in your user config, not in the repo). Without the `claude` CLI it writes `.mcp.json`
   and pre-approves it in `.claude/settings.local.json`.
2. **Hooks** in `.claude/settings.local.json` (git-ignored):

   | Event | Behavior |
   |---|---|
   | `PreToolUse` Read on images/PDFs | Read locally (vision model / PDF text). Claude gets the text; no image tokens. |
   | `PreToolUse` Read on large files | Returns an outline (signatures + line ranges) and suggests `brain_slice` / `offset`+`limit`. |
   | `PreToolUse` Grep / Glob | When results exceed the threshold: ranked top results (definitions first, tests and generated files last) + `raw_id`. |
   | `PostToolUse` Bash | Long output is compressed (`updatedToolOutput`). Error/warning lines, stack traces, failing test names and exit codes are never removed. Read-like commands (`cat`, `git diff`, …) are left alone unless enormous. |
   | `SessionStart` | A brief of ≤ ~500 tokens: overview, entry points, recent decisions, open tasks, last session. |
   | `Stop` / `SessionEnd` | The transcript is summarized locally into memory in the background (decisions, changed files, open tasks). |

   **Escape hatch:** when a hook replaces a Read/Grep/Glob result, Claude can simply repeat the
   identical call and it goes through unchanged. Hooks never break the agent: on any internal
   error they stay silent (logged to `~/.local-brain/logs/hooks.log`).
3. **`CLAUDE.md`** gets a short, marked section asking Claude to use `brain_query` first and
   `brain_slice` before editing.

### MCP tools

Tool schemas cost tokens on every request, so there are only five, with short descriptions:

| Tool | Input | Output |
|---|---|---|
| `brain_query` | `question`, optional `scope` | Short answer, `file:line` pointers, confidence and layer; `stale: true` when files changed after indexing |
| `brain_slice` | `path` + `symbol` or `lines` | Only that code, with line numbers |
| `brain_doc` | `path`, optional `question` | Locally read image/PDF content or a focused answer |
| `brain_raw` | `raw_id`, optional `grep` / `range` | The relevant part of a compressed output |
| `brain_memory` | `action: read \| note \| done` | Brief + decisions + tasks / record a decision, task or note / close one |

## Models and hardware tiers

On first `init`, local-brain detects RAM, Apple Silicon / NVIDIA GPU, free disk and running
runtimes, then picks a tier from the versioned catalog [`models.toml`](models.toml) (copied to
`~/.local-brain/models.toml`, edit freely). Model memory is kept under ~50% of RAM so your IDE,
browser and compiler still have room.

| Tier | Typical hardware | Models (Ollama defaults) |
|---|---|---|
| **S** | ≤ 16 GB RAM, or CPU only | `gemma3:4b` (text + vision) + `nomic-embed-text` |
| **M** | 24–32 GB | `gemma3:12b` (text + vision) + `nomic-embed-text` |
| **L** | ≥ 48 GB or a 24 GB+ GPU | `qwen2.5-coder:32b` (code) + `gemma3:27b` (vision/general) + `nomic-embed-text`, loaded on demand |

Override anything in `~/.local-brain/config.toml` (`[models]`), switch runtimes with
`local-brain doctor --set-provider lmstudio`, or re-profile with `local-brain init --reconfigure`.
Idle models are released after 10 minutes (`keep_alive`, configurable).

## Configuration

Global settings live in `~/.local-brain/config.toml` (created by `init`); per-project overrides
in `<project>/.local-brain/config.toml`. Highlights:

```toml
[runtime]
provider = "ollama"            # ollama | lmstudio | mlx | llamacpp | openai | none
base_url = "http://127.0.0.1:11434"
keep_alive = "10m"             # unload models after this idle time

[compress]
threshold_tokens = 2000        # outputs at or below this pass through untouched
target_tokens = 1200
llm_summary = true             # use the local LLM when rules are not enough
raw_ttl_days = 7               # how long raw outputs are kept for brain_raw

[hooks]
large_file_lines = 600         # Read of bigger files returns an outline first
large_file_tokens = 8000
grep_max_results = 80          # Grep results above this are ranked and trimmed
glob_max_results = 200
brief_tokens = 500

[index]
exclude = ["generated/", "*.snap"]   # never indexed (plus .gitignore, secrets, vendored dirs)
```

See [docs/configuration.md](docs/configuration.md) for every option.

## Privacy and security

* **Nothing leaves your machine.** The only network access is model downloads through your
  runtime (disable with `[network] allow_downloads = false`). Requests to the model runtime go to
  `localhost` and bypass HTTP proxies.
* **No ports are opened.** The MCP server talks over stdio; the daemon has no network listener.
* **Secret files are never indexed or read** (`.env*`, keys, certificates, credential files,
  Terraform state, …), in addition to everything in `.gitignore`.
* **Secrets in outputs are masked** (API keys, tokens, JWTs, passwords in URLs, private keys)
  before anything is shown to the agent or stored.
* **No telemetry.**

## Measuring savings

* **Live counter:** every tool call and hook records the tokens Claude would have seen without
  local-brain (an estimate: e.g. the files a pointer refers to, the full output, the image cost)
  and what it actually saw. Overheads such as the session brief count as costs.
  `local-brain status` shows the totals per day and per source.
* **`local-brain bench`:** derives 20 reproducible tasks from your project (seeded; editable in
  `.local-brain/bench/tasks.json`): "where is X defined", "who calls Y", architecture, document
  and test-suite tasks. Each configuration runs headless (`claude -p`) in its own disposable git
  worktree and is scored on total session tokens (input, output, cache read, cache write), tool
  calls, time, cost and task success. Pass `--graphify-setup "<command>"` with the command that
  installs graphify into a repository to include the graphify configuration. Reports are written
  as Markdown and JSON with seed and version information.

## Project status

This is the first public version (0.1). Implemented: everything in the CLI table above — code
graph for six languages, hybrid search, summaries with validation, architecture map, local
documents, compression, all five MCP tools, all hooks, memory, daemon with live re-indexing,
hardware profiling, runtime adapter, delegated generation, and the bench harness.

Differences from the original design, and what's next:

* **Runtimes:** local-brain talks to Ollama, LM Studio, MLX servers and llama.cpp's `llama-server`
  over their local HTTP APIs, and has a fully deterministic no-model mode. A llama.cpp engine
  *embedded* in the binary and a dedicated MLX sidecar are on the roadmap.
* **Storage:** vectors are stored in SQLite and searched exhaustively — fast for typical
  repositories (up to ~100k chunks). An ANN index is planned for very large monorepos.
* **Agents:** Claude Code today; the agent adapter layer is in place for Codex (phase 2).
* **Distribution:** prebuilt binaries for macOS (arm64/x64), Linux (x64/arm64) and Windows (x64)
  with SHA-256 checksums, install scripts and a Homebrew formula; the macOS binaries are signed
  and notarized. Windows code signing and inclusion in homebrew-core are planned.

## Troubleshooting

* `local-brain doctor` explains most problems and how to fix them.
* Hook activity and errors: `~/.local-brain/logs/hooks.log`; daemon: `~/.local-brain/logs/daemon.log`.
* Claude doesn't see the tools: run `/mcp` in Claude Code; re-run `local-brain init` if the binary
  moved (hooks and MCP use its absolute path).
* Summaries look generic: no model runtime was reachable during `learn`. Start your runtime and
  run `local-brain learn` again (only missing/changed work is redone).
* Want the old behavior for a moment? Repeat the same Read/Grep/Glob call, or set the relevant
  `[hooks]` option to `false`.

## Uninstall

`local-brain uninstall` removes the integration and index from every project, stops the daemon
and deletes `~/.local-brain`. Then remove the binary: `brew uninstall local-brain`, or delete
`~/.local/bin/local-brain` (Windows: `%LOCALAPPDATA%\local-brain\bin\local-brain.exe`).

## Releasing (maintainers)

1. Update the version in `Cargo.toml` and add a section to `CHANGELOG.md`.
2. Push a tag (`git tag v0.2.0 && git push origin v0.2.0`) — or run the **Release** workflow
   manually from the Actions tab with the new tag name.
3. The workflow builds all five platforms, signs and notarizes the macOS binaries (when the Apple
   secrets are configured, see [docs/macos-signing.md](docs/macos-signing.md)), and publishes the
   release with `SHA256SUMS` and notes from the changelog. Then update the URLs and checksums in
   `Formula/local-brain.rb`.

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Please report security issues
privately as described in [SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Copyright © 2026 AEOGONTECH LLC and
local-brain contributors.
