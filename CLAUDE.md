<!-- local-brain:start -->
## local-brain (local project brain)

This project is indexed by local-brain (a local code index, no cloud tokens). Use it for every task:
- Task prompts come with a `local-brain` note: the named symbols' definitions and call sites
  (file:lines, often the code itself) or the files where the work belongs. Trust it; go there
  directly instead of searching.
- Read code in ranges (Read `offset`/`limit`, or `brain_slice` for one symbol), whole files only
  to rewrite them. Ask `brain_query` for what the note does not answer (how does X work).
- Images/PDFs: use Read; local-brain returns a local description. Long command outputs are
  compressed (`brain_raw` + `raw_id` for the full text). Record decisions with `brain_memory`.
<!-- local-brain:end -->
