# Queue composer action and abort recovery

Reference pin: `9841914c71a74d81abe07f751aefd271fd924e63`.
Before implementation, inspected Pi's
`packages/coding-agent/src/modes/interactive/interactive-mode.ts`, especially
`restoreQueuedMessagesToEditor`: Pi restores steering/follow-up text to the
editor when aborting instead of leaving an unusable queue reference.

The reproduced Web UI bug has two causes: the square action always selects
Pause while busy, even during a held queue edit; after stopping, the composer
keeps the dead worker's edit ID and attempts to edit a nonexistent message.

Keep the existing single square action, but let a held edit select its save
action ahead of Pause. Editing then choosing Steer first promotes the still-held
item, then saves/releases it; existing ownership checks and FIFO/attachment
identity apply across both operations without a new control command.
Held steering edits block lower-priority follow-ups until released, so a
promoted draft cannot be overtaken while the save request is in flight. Missing
or invalidated edits become ordinary drafts with their text and attachment
references intact. Failed sends also retain the draft.

For abort recovery, return the worker's pending queue snapshot in its existing
Stop reply while holding the queue lock, then cancel. The Web UI restores the
pending text and attachments to the composer, following Pi's restore-before-
resend behavior. Add authenticated attachment-path metadata to queue state and
accept existing attachment references only after Rust verifies that each file
resolves inside this session's private attachment directory. These are the
smallest mechanisms needed by BashKitten's documented separate-worker/Web UI
and attachment design; they introduce no new model tool or queue mode.

Verification on the installed normal port-3939 application:

- All 134 native tests pass, including held-edit promotion ordering, Stop queue
  snapshots with attachments, and retained-file confinement. Strict all-target
  Clippy, Rust formatting, embedded JavaScript syntax and dpkg integrity pass.
- In the existing full-context Luna session, mouse-clicking the edit arrow
  saved the new queue text; editing then clicking Steer promoted the edited
  text. The native worker remained PID 598803 throughout.
- Removed a held queue item through its native control socket to reproduce a
  stale composer reference. The browser retained the changed draft and original
  attachment, restored ordinary composer controls, and Enter queued the draft
  successfully under a new ID. The retained path and bytes were unchanged.
- No additional abort occurred. The historical abort from the reproduced bug
  remains visible in session history; no history was rewritten.
- Installed Debian package SHA-256:
  `e2184f8a8d11fc6b45721c6704da6c13ea102e93b0c5e5d51831087f773f2648`.
  Deployment restarted only Web; the real full-context test continues.
