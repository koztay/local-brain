<!-- local-brain:start -->
## local-brain (local project brain)

This project is indexed by local-brain, a local index of this codebase that costs no cloud
tokens. Use it for every task (features, fixes, refactors, reviews), not only for questions:
- Prompts may arrive with a "local-brain context" note listing the most relevant files. Start there.
- To locate code for a task, ask `brain_query` (e.g. "admin panel pages and their layout components")
  before exploring with Glob/Grep/Read.
- Read only what you need with `brain_slice` (one symbol or a line range). Read a whole file only
  when you are about to change most of it.
- Image and PDF files are read locally: use `brain_doc` (or Read, which local-brain intercepts).
- Long command outputs are compressed; get the full text with `brain_raw` and the given `raw_id`.
- Record important technical decisions with `brain_memory` (action `note`).
<!-- local-brain:end -->
