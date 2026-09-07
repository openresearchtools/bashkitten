# Chat rename and confirmed deletion

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
