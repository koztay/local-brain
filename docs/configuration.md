# Configuration reference

local-brain reads `~/.local-brain/config.toml` (global, created by `local-brain init`) and
`<project>/.local-brain/config.toml` (per project). Set `LOCAL_BRAIN_HOME` to use another global
directory. Unknown keys are ignored; missing keys take the defaults shown here.

## `[runtime]`

| Key | Default | Meaning |
|---|---|---|
| `provider` | detected | `ollama`, `lmstudio`, `mlx`, `llamacpp`, `openai` (any OpenAI-compatible local server) or `none` |
| `base_url` | provider default | Ollama `http://127.0.0.1:11434`, LM Studio `http://127.0.0.1:1234/v1`, MLX / llama.cpp `http://127.0.0.1:8080/v1` |
| `keep_alive` | `"10m"` | How long the runtime keeps a model loaded after the last request (Ollama) |
| `ollama_models_dir` | `""` | Set by `init` when it installed Ollama with the models on another drive (passed to Ollama as `OLLAMA_MODELS`). Empty = Ollama's default |
| `timeout_secs` | `180` | Per-request timeout for model calls (hooks use shorter limits) |
| `api_key` | `""` | Sent as a bearer token, for servers that require one |

Switch with `local-brain doctor --set-provider <name> [--url <base url>]`.

## `[models]`

| Key | Default | Meaning |
|---|---|---|
| `tier` | detected | `S`, `M` or `L` (see `models.toml`) |
| `general` | from catalog | Main model: summaries, compression, answers; also vision when `general_has_vision` |
| `code` | from catalog | Optional dedicated code model (tier L). Empty = `general` |
| `vision` | from catalog | Optional dedicated vision model. Empty = `general` if it has vision |
| `embedding` | from catalog | Embedding model. Empty = built-in hash embeddings (`hash-v1-512`) |
| `general_has_vision` | from catalog | Whether `general` accepts images |

Changing the embedding model triggers re-embedding on the next `local-brain learn`.

## `[compress]`

| Key | Default | Meaning |
|---|---|---|
| `threshold_tokens` | `2000` | Outputs at or under this size pass through untouched |
| `target_tokens` | `1200` | Target size of compressed outputs |
| `llm_summary` | `true` | Use the local model when deterministic rules are not enough |
| `raw_ttl_days` | `7` | How long raw outputs stay available for `brain_raw` |

Error and warning lines, stack traces, failing test names and exit codes are never removed,
regardless of these settings.

## `[hooks]`

| Key | Default | Meaning |
|---|---|---|
| `bash` | `true` | Compress long Bash output (PostToolUse) |
| `read_docs` | `true` | Read images/PDFs locally instead of sending them to the cloud model |
| `read_large` | `true` | Return an outline for large files |
| `grep` / `glob` | `true` | Rank and trim oversized search results |
| `session_brief` | `true` | Inject the project brief at session start |
| `session_memory` | `true` | Summarize sessions into memory |
| `prompt_context` | `true` | Add where-the-code-lives context to task prompts (UserPromptSubmit) |
| `read_outside_docs` | `true` | Also read images/PDFs outside the project locally (screenshots in `~/Desktop`, …) |
| `large_file_lines` | `600` | Line threshold for "large file" |
| `large_file_tokens` | `8000` | Token threshold for "large file" |
| `grep_max_results` | `80` | Grep results (lines or files) above this are trimmed |
| `grep_show` | `30` | How many ranked results are shown |
| `glob_max_results` | `200` | Glob matches above this are trimmed |
| `brief_tokens` | `500` | Budget of the session brief |
| `prompt_context_tokens` | `600` | Budget of the per-prompt context note |

Bash outputs of test/build/lint/install/log commands are compressed above `threshold_tokens`;
other commands above 2× that; read-like commands (`cat`, `head`, `git diff`, …) only above 10×.

## `[learn]`

| Key | Default | Meaning |
|---|---|---|
| `llm_summaries` | `true` | Use the local model for file/module/project summaries |
| `max_llm_files` | `300` | How many files get model-written summaries: the most referenced code files first; the rest keep instant rule-based summaries |
| `max_file_bytes` | `1000000` | Larger text files are skipped (documents may be 50× larger) |
| `max_images` | `50` | Images described per `learn` run |
| `embed_batch` | `32` | Embedding batch size |

## `[index]`

| Key | Default | Meaning |
|---|---|---|
| `exclude` | `[]` | Extra gitignore-style patterns. Global and project lists are combined |

Always excluded: everything in `.gitignore`, `.git/`, `.local-brain/`, `node_modules/`,
`target/`, `dist/`, `build/`, virtualenvs, vendored code, lockfiles, minified files, and secret
files (`.env*`, `*.pem`, `*.key`, `id_rsa*`, credential/secret JSON/YAML, `*.tfstate`, …).

## `[daemon]`

| Key | Default | Meaning |
|---|---|---|
| `debounce_ms` | `500` | Quiet time before a batch of file changes is processed |
| `idle_unload_minutes` | `10` | Release model memory after this much inactivity |

## `[network]`

| Key | Default | Meaning |
|---|---|---|
| `allow_downloads` | `true` | Allow `init` to install Ollama and download models through it |
| `check_updates` | `true` | Ask the GitHub releases API once a day whether a newer local-brain exists (also `LOCAL_BRAIN_NO_UPDATE_CHECK=1`) |

## Project files

* `.local-brain/brain.db` — the index (SQLite, WAL mode)
* `.local-brain/raw/` — raw outputs behind `raw_id`s (expire after `raw_ttl_days`)
* `.local-brain/memory.md` — human-editable memory; your edits take precedence
* `.local-brain/bench/` — benchmark tasks and reports
* `.local-brain/config.toml` — project overrides (`[index]`, `[compress]`, `[hooks]`)

## Environment variables

* `LOCAL_BRAIN_HOME` — global directory (default `~/.local-brain`)
* `CLAUDE_PROJECT_DIR` — set by Claude Code; used to locate the project
* `LOCAL_BRAIN_NO_CLAUDE_CLI` — do not call the `claude` CLI during `init` (writes `.mcp.json` instead)
