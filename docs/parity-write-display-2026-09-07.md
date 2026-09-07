# Write display and folder-action contrast

Pinned Pi reference: `9841914c71a74d81abe07f751aefd271fd924e63`.

Before implementation, inspected
`packages/coding-agent/src/core/tools/renderers/write.ts` at that commit.
`formatWriteCall` retains `args.content` and displays the complete content when
expanded. `formatWriteResult` suppresses successful write-result text and adds
error text only on failure. The original tool result in
`packages/coding-agent/src/core/tools/write.ts` remains model-visible.

BashKitten's Web UI currently deletes streamed arguments when the completed
call arrives, then replaces the entire tool body with the result text. Retain
the write call's exact content in the existing scrollable tool body, including
when reconstructing history. Display completion status on the compact summary
row, and show failed-write errors alongside the attempted content. This uses
the documented Web UI presentation difference; tool messages and persisted
history are unchanged. Render content as text, never HTML.

The shared folder dialog's generic action-button rule also overrides the
primary button background, leaving white text on a light dialog until hover.
Restrict the transparent background to secondary actions so primary actions
use the existing theme palette in their normal and hover states.

## Verification

The normal installed Web UI at port 3939 was checked during the real Luna
website build, including an authenticated browser reload. All 14 completed
write calls retained content exactly equal to their recorded arguments, with
success on the summary row and no duplicate success-result text. The 25,279
character `db.py` call expanded to a scrollable pane (315 pixel client height,
7,004 pixel scroll height). A screenshot confirmed the full code presentation.
Existing failed calls displayed `Failed` on the summary row. No browser errors
or warnings were recorded in the verification tab.

The light-mode folder button previously had white text and a transparent
background. After installation, its non-hovered background was RGB(95,95,101)
with white text, and the screenshot confirmed it was visible.

Embedded JavaScript syntax, all 131 Rust tests and package installation checks
passed. Debian package SHA-256:
`043059d5f7865b112c465fd915c54d08ef7910fd39d2a55fcc6be1980ca1bb07`.
Only the Web service restarted; the active website worker retained PID 511812
and its unchanged 272,000-token model context and compaction configuration.
This verifies the UI corrections; the long-run automatic-compaction test is
still in progress and is not included in this pass claim.
