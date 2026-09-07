# Requested native downloads and session goals

Decision recorded before implementation. Pi reference remains
`9841914c71a74d81abe07f751aefd271fd924e63`.

The user explicitly requests these BashKitten differences on 2026-09-07:

- Integrate the full native Rust download engine from their SimpleHF repository,
  not a symlink, subprocess dependency or cache-name indirection. Source pin:
  `a7dccac659e4ee71d0652807f198eda20d0e7cdc`, `engine/main.rs` and the paginated
  repository listing in `gui/main.rs`. Preserve adaptive ranged transfers,
  bounded connections, progress, pause/resume, retries, selected paths and
  original file names. The Web UI selects repository files and a normal download
  folder (default `~/.local/share/bashkitten/models`); complete downloads join
  normal local model discovery. Temporary part files remain resumable. Add an
  optional private saved HF token, never exposed in settings responses, plus a
  memory-only token for individual requests. Keep the existing Pi router
  workflow available; native downloads do not require a running router.
- `/goal <objective>` persists one active session goal and continues ordinary
  turns toward it until completed or paused. This is an explicitly requested
  addition absent from pinned Pi. Use custom logical session events and the
  existing control socket; add no model tool. The agent can inspect and finish
  the goal with `bashkitten goal ...` via `bash`, using its existing session ID
  environment. User Pause, manual compaction, cancellation and terminal errors
  suspend automatic continuation rather than creating an unstoppable loop.
  Queued user work retains priority; no timer, scheduler or new process exists.
- Make the existing Pi manual `/compact` command work from the composer at any
  active/idle stage. Pi `AgentSession.compact()` first aborts the current
  operation, then summarizes without continuing that interrupted turn. Reuse
  this native path unchanged and expose a completed-compaction count in the
  per-chat stats. Count logical compactions once across numbered segments and
  preserve the value through restart and fork; no separate UI estimate.

SimpleHF and rust-hf-downloader MIT notices accompany the integrated source.
Adaptation to in-process callbacks and cancellation replaces its stdin/stdout
protocol; all network calls remain attributable to an explicit download/search
request. HTTP fixtures must verify authenticated listing/pagination, ranges,
resume, cancellation, filename preservation and credential redaction.

The composer plus menu exposes Attach files, Goal and Compact context. Goal
adds a removable chip to the ordinary message; no modal or second text editor
is used. The message and goal are submitted in one native control operation,
including attachments, so automatic continuation cannot race ahead of the
original message. A goal ID in the agent completion command rejects completion
of a replacement goal. While active, the goal guidance is appended to the
ordinary system prompt; summarization prompts remain byte-exact Pi prompts.

Compaction progress includes the actual generated summary text as native SSE
`compaction_delta` events, grouped by summary request (including split-turn
summaries and retries). The active work trace retains coalesced partial text
for reconnect. The final persisted Pi compaction summary replaces that live
pane, remaining expandable and scrollable. A native `compacting` snapshot field
keeps its active state visible after reload; orderly worker EOF clears stale
running controls without touching the composer draft.
