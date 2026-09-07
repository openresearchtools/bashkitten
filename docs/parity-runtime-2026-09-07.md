# Runtime completion audit — 2026-09-07

Behavioral reference remains Pi `9841914c71a74d81abe07f751aefd271fd924e63`;
the separate checkout HEAD was verified before implementation.

Required architecture corrections, recorded before implementation:

- SSE subscriptions are read-only. An offline subscription reports offline;
  resuming a worker before subscribing is an authenticated, Origin/CSRF-checked
  POST. This implements the existing local Web authentication requirement.
- Concurrent composer edits need ownership of the existing FIFO hold. A
  per-edit identifier in the existing queue request prevents a stale tab from
  saving or releasing another tab's hold. It is transient UI plumbing, not
  persisted session metadata or a new agent tool. Removal remains available.
  Clicking Edit on a visibly held row can transfer that hold to the new
  composer; the former owner's save/cancel is rejected. This recovers edits
  after a closed/reloaded tab without a persistent lease store or timeout that
  could unexpectedly deliver a message. Only one begin-edit request may be in
  flight per composer.
- Failed append/publication must leave the last complete history boundary
  usable; failed model changes must not activate an unpersisted selection.
- Private session files must have their permissions corrected before reading,
  and new sensitive files must be private from creation.
- A numbered segment checkpoints the latest omitted assistant's cache-hit rate
  in its header when necessary. Pi's footer reads all historical assistants;
  keeping this value allows the same footer after loading only the newest file.
  Accumulate omitted and retained usage in chronological order so floating-point
  cost addition follows Pi rather than adding pre-summed groups in reverse order.
- Port changes persist the previous port as a transient field in the existing
  configuration until the replacement Web process successfully binds. If the
  destination port was taken after preflight, that process binds the previous
  port and removes the transient field. The field is hidden from settings forms;
  this uses the existing Web service and adds no process supervisor or index.
- Session JSONL readers and the session writer use shared/exclusive file locks
  during completed-turn appends so readers see a complete old or new boundary.
- The model selector waits for worker state rather than treating an acknowledged
  queued request as a successful switch. Status and subscriptions include the
  active selection, including after rejected pre-switch compaction.

## Verification

The native worker tests cover failed model persistence with an actual filesystem
failure, failed/aborted summaries and pre-switch compaction, concurrent edit
ownership/takeover, UTF-16 live replay and chronological usage checkpoints.
`tests/session_boundaries.rs` adds real shared/exclusive-lock blocking tests,
older-segment and fork permission repairs, and saved usage after a preset is
removed. Context falls back to the saved header only when its model still matches
the effective selection.

Actual loopback HTTP tests verify that GET events never starts a worker; resume
requires authentication, matching Origin and CSRF; logout closes an existing SSE
stream before another private event is delivered. Port recovery tests cover both
a failed restart command and a port taken between preflight and replacement bind.

`tests/pi_usage_checkpoint.rs` compares actual pinned Pi footer totals against
native compaction checkpoints, including zero-token latest replies and a floating
point addition-order boundary.

Browser acceptance using the fresh native CLI, worker and embedded Web UI verified
signup/reload, live reasoning and tool output, held-message takeover after reload,
FIFO delivery, model/context changes, authoritative usage, partial-output Pause
and reload, historical fork selection and resumed execution. The browser console
had no warnings/errors. See `tests/live/2026-09-07-browser-runtime.json`.
The temporary browser/provider servers and their session services were stopped
after those checks; they were not installed as another application.
