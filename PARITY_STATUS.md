# Pi parity repair — active, not complete

Reference: Pi `9841914c71a74d81abe07f751aefd271fd924e63`, as recorded in
`PI_UPSTREAM.md`. `AGENTS.md` remains the specification; this ledger is evidence,
not a reduction in scope. Passing isolated helper tests is not completion.

## Required completion gates

All rows start unverified. Mark a row complete only with current source evidence,
equivalent pinned-Pi fixtures where applicable, and tests of the actual runtime
and exposed UI/CLI paths. Keep intentional differences limited to `AGENTS.md`.

| Area | Required implementation and verification | Status |
| --- | --- | --- |
| Compaction | Manual, threshold, overflow, pre-switch; exact prompts/preparation/summary usage; abort/failure; numbered-file rotation, resume and continuation | Pending |
| Retry | Pi error classification, delays, limits, cancellation, stream-idle timeout and continuation | Pending |
| Cancellation | Abort provider/tools, preserve partial response and settled history, cooperative stop and target shutdown | Core worker flow implemented; fixture tests pass; signal/deployment/Pi differential checks pending |
| Tool streaming | Argument lifecycle, incremental bash output, individual tool completion, ordered context | Output/completion connected and runtime-tested; argument UI and differential checks pending |
| Seven tools | Schemas, wording, coercion, paths, image processing, cancellation, errors, truncation and edge cases against pinned Pi | Pending |
| Prompt/context | Pi system prompt construction, project instructions and overrides; documented passive skills difference | Pending |
| Codex | Ordered Responses items/signatures; exact replay, transport/cache/affinity, refresh, errors, images, native OAuth and logout | Indexed item preservation implemented/tested; other gaps and differential checks pending |
| Compatible API | All applicable Pi compatibility settings/transforms, image tool results, thinking, streaming, usage, model presets | Tool-result image grouping/capability filtering implemented/tested; remaining parity pending |
| Usage | Worker-owned totals, context, cache, costs, tiers, compaction resets/unknown values; UI renders authoritative values | Pending |
| llama.cpp | Debian detection, router lifecycle and args, catalog/load/unload/retain, HF search/quantization/download/cancel/gated/retry, model metadata | Pending |
| Live reconnect | Atomic persisted-history/live snapshot and future stream without gaps or duplication | Snapshot and UI replay wired; late-subscriber fixture passes; browser/race checks pending |
| Provider settings | Complete compatible provider/model and llama.cpp controls; inspectable launch arguments | Pending |
| Sidebar | Cheap header/title/stat listing; actual paginated scroll; no history scans or secondary index | Pending |
| Queue editing | Held edit preserves FIFO position and attachments; safe edit/cancel/promote/remove boundaries | Pending |
| Lifecycle | GTK normal quit/SIGTERM, crash restart, startup toggles, web-only port restart, no interrupted sibling agents | Pending |
| Session/model invariants | Parent cwd inheritance, shared validation, effective non-secret parameters in header, safe model switches, forks/attachments | Pending |
| Delivery | Full tests, Podman .deb build, install, browser/live-model testing, concurrent sessions, no secrets in Git, commit and push | Pending |

## Evidence and work log

- 2026-09-05: Read the user-provided gap list; confirmed clean baseline
  `6bbf97c`. Repair started. No row is certified by the earlier 46 helper tests.
- 2026-09-05: Added indexed response assembly and parser integration fixtures:
  distinct text/reasoning items retain their own signatures, including partial
  output on abort/error. Compatible tool-result images follow the complete
  consecutive tool-result batch, and current model capability filters images in
  both provider paths. Pi sources: `packages/ai/src/api/openai-completions.ts`,
  `openai-responses-shared.ts`, `openai-codex-responses.ts` at the pinned commit.
- Connected tool output callbacks and per-tool completion; kept result messages
  in call order. Worker control cancellation now aborts streaming/tools and
  flushes before removing its socket. Worker SIGTERM/INT and GTK SIGTERM/shutdown
  handlers added. These systemd/GTK paths still require installed-process tests.
- Added coalesced uncommitted-turn replay under the same lock as publication and
  subscription. Browser deduplicates message entry IDs and reloads the disk
  boundary on automatic stream reconnect. Browser/race verification remains open.
- Podman `cargo test --locked`: 52 passed. New actual-worker tests use a local
  HTTP streaming fixture and Unix control sockets to check: late subscription,
  unflushed live output, partial-answer persistence on stop, live bash output,
  tool cancellation and preservation of a still-running sibling worker.
  Embedded JavaScript parses successfully with Node's `vm.Script` (development
  check only; no Node dependency added to the application or package build).
- No deployment/restart or real-provider request was performed for this repair
  checkpoint. Earlier installed build must not be confused with the new source.

## Next critical path (still required, not deferred out of scope)

1. Wire manual/automatic/overflow/pre-switch compaction and retry into the worker.
   Pinned `agent-session.ts` lines 1930–2430 defines manual abort-first semantics,
   stale-usage checks, threshold vs overflow, single overflow recovery and queue
   continuation. `compaction/compaction.ts` defines sequential history/prefix
   summarization, exact budgets/prompts, usage combination and file-operation
   suffix. Summary requests disable cache retention; retain routing as Pi does.
2. Implement atomic numbered JSONL rotation and resume using `build_session_context`
   (now used at worker startup). Preserve Pi entry IDs/logical context and immutable
   old segments. Verify disk failure/cancel leaves old context usable, and preserve
   usage across segment rollover without double-counting retained messages.
3. Continue every remaining matrix row; create differential fixtures against the
   exact pinned Pi, not merely Rust fixtures whose expected values look plausible.
   The external reference checkout is still v0.84.4: use `git show <pinned-commit>`
   until a separate pinned checkout is created. Do not silently test against that
   older working checkout or the older system-installed Pi.
