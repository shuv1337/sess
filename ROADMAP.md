# Roadmap

This roadmap reflects the current implementation state and the highest-impact next steps.

## P0 — Correctness & Data Safety

1. **Fix incremental stale deletion semantics**
   - Ensure incremental scans do not delete conversations that were simply not rescanned.
   - Add targeted tests for mixed old/new file sets.

2. **Preserve semantic-only hybrid hits**
   - In RRF merge, materialize result rows for candidates found only by semantic retrieval.
   - Add tests proving semantic-only recall.

3. **Improve incremental embedding refresh behavior**
   - Ensure insert-only incremental runs still produce embeddings for new conversations.

## P1 — UX / Feature Completeness

4. **Enable semantic/hybrid retrieval in TUI path**
   - Add semantic toggle and merge path to background TUI search thread.
   - Surface semantic state in status bar/help.

5. **Richer snippets and highlighting**
   - Improve snippet extraction and term highlighting stability.
   - Avoid naive substring replacement edge cases.

6. **Safer workspace/source matching tools**
   - Improve path filtering and possible fuzzy/exact matching options.

## P2 — Code Quality & Reliability

7. **Warning cleanup pass**
   - Reduce/resolve compiler warnings (unused imports/dead code/etc.).

8. **Add CI pipeline**
   - Run `fmt`, `clippy`, `test` on PRs.

9. **Document operational guarantees**
   - Clarify indexing invariants and failure modes.

## P3 — Performance & Scale

10. **Semantic search scaling**
    - Evaluate ANN/vector index approach for larger corpora.

11. **Connector scan efficiency**
    - Reduce redundant IO and improve incremental file inventorying.

12. **Optional parallel indexing improvements**
    - Evaluate controlled concurrency in non-OpenCode paths.

## Nice-to-have

- Export/import index metadata for portability
- Saved search presets
- Optional conversation tags/bookmarks
