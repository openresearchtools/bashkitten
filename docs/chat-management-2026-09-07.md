# Chat rename and confirmed deletion

## Secondary-click and project deletion decision, 2026-09-08

At the user's explicit request, chat actions live in a secondary-click context
menu, with no separate ellipsis button. Keyboard Context Menu / Shift+F10 opens
the same menu. Folder headings offer Delete project chats. Confirmation lists
the exact recorded working folder and the total chat count, including chats not
yet loaded in the sidebar. Project groups remain derived from session headers;
there is no project storage folder or secondary index to delete.

The server previews all matching session IDs and requires that confirmed set on
deletion. It refuses changed group membership, locks and validates every selected
session, and stops all selected workers before removing any session storage.
Only those ID-derived session directories are removed. Working directories,
repositories, other groups and nested-folder groups are retained. Any newly
created chat after validation is outside the confirmed set and is retained.
These narrowly scoped UI/storage differences extend the user's confirmed-delete
request; Pi's pinned behavior and all agent/provider behavior stay unchanged.

Decision recorded before implementation at the user's explicit request on
2026-09-07. Pi remains pinned to `9841914c71a74d81abe07f751aefd271fd924e63`.
Its `session-selector.ts` provides rename and confirmed session-file deletion;
BashKitten applies these to its documented title file and per-session directory.

Rename atomically replaces only the one-line private title file. Delete requires
a chat-specific confirmation in the Web UI, then gracefully stops the selected
worker and removes that session directory, including numbered JSONLs and copied
attachments. This permanently removes the directory instead of Pi's optional
trash-command path, as explicitly requested. It never resolves the deletion
target from the working-directory header or attachment references. Symlinked
session directories, invalid IDs, and mismatched headers are refused. A working
directory inside the session directory is refused rather than deleted. Directory
locks serialize rename, deletion, worker launch and Web attachment publication;
no session index, new supervisor or model tool is added.

Both endpoints require login, exact Origin and CSRF. Cancelling confirmation
makes no request. A failed stop leaves the folder intact. The UI removes only the
deleted sidebar row and clears the chat view when that chat is open.

## Verification completed 2026-09-08

Native tests pass for title-only rename, private title permissions, selected
folder deletion, preservation of working files/sibling sessions, rejection of
symlinked session roots and nested working directories, and unlinking contained
attachment symlinks without touching their destinations. A running native worker
executing a long bash call is cancelled and awaited before its directory is
removed. HTTP tests reject missing login, missing/foreign Origin/CSRF and an
unconfirmed delete, then verify the confirmed endpoint removes the directory.

Installed-browser checks renamed the Goal acceptance chat, reloaded its title,
opened Delete chat, confirmed both buttons are visible in light mode and
cancelled without removing the chat. The existing conversation's work continued
through the Web-only package restart.

The secondary-click revision passed 157 native tests and strict Clippy. Group
tests cover exact folder membership, stale/duplicate/cross-group confirmations,
working-folder and nested-group preservation, and a different chat whose working
folder is inside selected storage. The authenticated HTTP fixture previews and
deletes 103 chats, exceeding a sidebar page, and rejects missing authentication,
CSRF, foreign Origin and unconfirmed requests.

Installed-browser checks opened chat and project menus with secondary-click and
Shift+F10. A disposable chat rename survived reload. Project confirmation showed
the full folder, two-chat count, Cancel and a visible destructive button in light
mode. Confirming removed both disposable session directories and attachments,
and removed their sidebar group. A sentinel file in their working folder and the
unrelated full-context test conversation remained intact. No provider calls or
additional app instances were needed for these disposable storage fixtures.
