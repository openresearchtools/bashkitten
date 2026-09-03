# BashKitten Agent Instructions

## Fixed built-in tool surface

BashKitten must expose exactly these seven built-in tools:

- `bash`
- `read`
- `edit`
- `write`
- `grep`
- `find`
- `ls`

Do not add an eighth built-in tool. Subagents, skills, package installation, scripts, Web UI operations, and every other capability must use these seven tools, ordinary files, or BashKitten CLI commands invoked through `bash`. External programs invoked through `bash` do not become BashKitten tools.

## Exact Pi tool parity

The seven built-in tools must be cleanly rewritten in Rust with exact 1:1 functional parity with the corresponding tools in Pi. Pi is the behavioral specification; BashKitten must not redesign, simplify, extend, or "improve" their behavior.

Before implementing or updating the tools, pin and record the exact upstream Pi commit used as the reference. For that pinned version, preserve all of the following exactly:

- Tool names.
- Model-visible tool descriptions.
- Argument names, schemas, required fields, optional fields, and defaults.
- Tool-specific prompt guidelines.
- Default system-prompt text that explains or governs the tools.
- Any default skill or other model-visible guidance Pi uses for these tools.
- Path handling and working-directory behavior.
- Execution behavior, cancellation behavior, and exit-code handling.
- Output formatting, truncation, limits, and metadata.
- Error conditions, error wording, and edge-case behavior.

All model-visible wording listed above must be copied verbatim from the pinned Pi source rather than summarized or paraphrased. The implementation itself must be native Rust and must not require Pi, Node.js, npm, or Pi's JavaScript/TypeScript runtime at build or runtime.

Parity must be checked with fixtures that run equivalent calls against the pinned Pi implementation and the Rust implementation and compare their observable results. A tool is not complete merely because its normal case works; it is complete only when its schema, prompting, normal behavior, failures, cancellation, and edge cases match Pi.

## System components and lifecycle

BashKitten consists of three distinct components with separate process lifetimes:

1. Agent session processes.
2. The Web UI server.
3. The GTK system controller and optional tray.

The GTK system controller is the lifecycle authority for one BashKitten instance. The Web UI and agent sessions remain independent sibling processes rather than running inside the controller or inside one another. Use systemd user services and a shared BashKitten target to track those processes; do not build a second process supervisor.

### Agent session processes

Every active agent session is its own system process and owns its own in-memory agent state, live response stream, control socket, message queues, and numbered JSONL files. Agent sessions must run with or without the Web UI running. A controller restart must be able to rediscover all session services that systemd is already tracking.

Stopping, restarting, or crashing the Web UI must not stop any agent session. One agent session crashing must not stop any other agent session. Finished sessions remain available from their JSONL files and can be listed and opened later. Explicitly quitting BashKitten through the controller must gracefully stop the Web UI and every running agent session so no processes are left behind.

### Web UI server

The Web UI is a completely separate process from the agent sessions and GTK controller. It is only a user interface into sessions and must not contain, own, or execute the agent runtime.

Implement the Web UI as plain HTML, CSS, and JavaScript embedded into the Rust Web UI server binary. Do not require Node.js or npm at build or runtime, and do not introduce a large frontend framework.

Its responsibilities are limited to:

- First-time signup and subsequent login for one local Web UI user.
- Listing running and finished sessions.
- Creating, opening, resuming, viewing, steering, queueing, and stopping sessions.
- Presenting each session primarily as a chat view.
- Loading the newest numbered JSONL immediately.
- Loading older JSONLs incrementally when the user scrolls upward.
- Reading completed session history directly from the numbered JSONLs.
- Connecting to a running session's control socket and combining completed JSONL history with its live in-memory stream.
- Sending text messages and attached photos or screenshots to sessions.
- Displaying normal assistant output and calls and results from the seven built-in tools.
- Displaying Pi-equivalent token consumption, context percentage, input and output tokens, cache usage, cache statistics, and cost values supplied by the agent process.

The Web UI must bind to `127.0.0.1` by default. Its port comes from BashKitten's local configuration. Changing the port restarts only the Web UI server and must not interrupt agent sessions.

Closing the browser or stopping, restarting, or crashing the Web UI server must not stop any running agent. Agents continue working without a browser or Web UI process.

The Web UI has exactly one local username-and-password account. When no Web UI user exists, it shows first-time signup and allows the first local visitor to choose the username and password. Store only the password hash. After signup, normal visits show the login screen.

Resetting the Web UI user removes the local login identity and invalidates existing Web UI logins. It must not delete session JSONLs, attachments, skills, configuration unrelated to authentication, or running agent processes. The next Web UI visit returns to first-time signup.

The Web UI must not contain a sophisticated file browser, worktree manager, plugin marketplace, MCP interface, npm package system, or unrelated dashboard features. Provider login and model configuration are not part of the current Web UI scope unless they are explicitly specified later.

### GTK system controller and tray

Provide a small GTK application for system settings and lifecycle control. It is not the Web UI, does not run agents, and does not contain an agent session browser.

Its settings window contains only the necessary controls:

- Start BashKitten automatically at desktop login.
- Automatically restart the Web UI server after a crash.
- View and change the Web UI port.
- Reset the local Web UI user so the next visit performs first-time signup.
- Open the Web UI in the default browser.
- Show basic About and license information.

The automatic-restart toggle controls the Web UI service's systemd restart behavior. The start-at-login toggle controls whether the BashKitten controller and its systemd target start at desktop login.

The controller must treat an explicit Quit, a normal application exit, or `SIGTERM` as a request to stop the complete BashKitten target. The controller itself must use systemd restart-on-failure behavior for abnormal crashes. After a crash, systemd restarts it and the restarted controller rediscovers the Web UI and agent-session services already tracked under the BashKitten target. Those processes are therefore not orphans and must not be tied directly to the GTK process ID.

The GTK controller may also provide a system tray icon. Choosing Settings from the tray must open the same GTK settings window; do not create a separate tray-only settings interface. When the tray is enabled, closing the settings window hides it while the controller keeps running. Choosing Quit from the tray or settings window stops the complete BashKitten systemd target, including the Web UI and all agent sessions, and then exits the controller. When the tray is disabled, closing the controller window performs the same complete quit.

## Session storage and memory lifecycle

Store every session in its own directory. Session history is split into monotonically numbered JSONL files:

```text
sessions/<session-id>/
├── title
├── 000001.jsonl
├── 000002.jsonl
├── 000003.jsonl
├── control.sock
└── attachments/
```

Do not create `meta.json`. Derive session information from the directory and files that already exist:

- The session ID is the directory name.
- The highest-numbered JSONL is the current segment.
- The current segment's modification time is the session's last completed activity time.
- A live session process or control socket means the session is running.
- No live session process means the persisted session is finished.
- The numbered JSONLs are the complete session history.

The only sidebar-specific sidecar is `title`, a plain text file containing one line. Initially write a shortened form of the first user message. Make one secondary model call using the first user message to generate a short conversation title, then overwrite `title` with that result. Generate a title only once; never make title-generation calls while listing or loading the sidebar. If title generation fails, retain the shortened fallback title. The user or agent may edit the file directly. Record the title call's token usage in the current JSONL so its real provider cost remains visible, but do not include title-generation content in the agent context or compaction calculation.

To populate the sidebar cheaply:

1. List the session directories.
2. Find each directory's highest-numbered JSONL.
3. Use that JSONL's modification time for recency sorting.
4. Read the one-line `title` file.
5. Check whether the session process or control socket is active.
6. Return only the requested newest page and load more as the user scrolls.

The Web UI may cache these derived entries in memory while it runs. Do not add a metadata schema, database, global index, or JSONL history parsing merely to populate the sidebar.

The highest-numbered JSONL is the current segment. Every completed compaction closes the current segment and creates exactly one new numbered JSONL. Older segments are immutable history.

The active session process keeps the current run in memory. Provider token deltas and partial assistant or tool output are streamed from that memory and are not written token by token to JSONL. Persist the in-memory session state at only these normal boundaries:

- The complete agent turn has settled.
- Compaction completes and produces the next numbered JSONL.

At a completed-turn boundary, append the completed entries accumulated since the previous boundary to the current JSONL and flush them. Do not rewrite older entries. At a compaction boundary, persist the completed pre-compaction state, close the current JSONL, and write the compacted active context into the newly numbered JSONL.

When a session process starts or resumes, load the highest-numbered JSONL into memory immediately. Do not load older segments into the agent's active context. If no compaction occurs, the next user, steering, or queued message continues from the current in-memory context and the newest JSONL. After compaction, replace the in-memory context immediately with Pi's compacted context, create the next numbered JSONL, and use that compacted context for the next model request.

The Web UI initially reads only the highest-numbered JSONL. For a running session, it combines that completed on-disk history with the live in-memory stream received from the session control socket. When the user scrolls upward, load preceding JSONL files in descending order as needed. Do not concatenate or parse every historical segment before showing the current session.

The numbered-file layout is BashKitten's deliberate storage difference from Pi. It must not change the logical conversation, compaction result, model context, or usage accounting.

## Exact Pi compaction parity

Compaction is a strict compatibility boundary. Use the same pinned upstream Pi commit used for tool parity, and rewrite its compaction implementation in Rust with exact 1:1 behavioral parity. Do not design a new summarizer or adjust Pi's behavior.

Preserve exactly for the pinned Pi version:

- Automatic-compaction enablement and defaults.
- The exact context-window, reserve-token, and threshold calculations that decide when automatic compaction runs.
- Every token-estimation and fallback calculation used by the trigger and cut-point logic.
- The exact backward walk, cut-point selection, recent-token retention, and message selection behavior.
- Handling of an earlier compaction summary and iterative compaction.
- Conversion and serialization of conversation content for summarization.
- The complete compaction system prompt, user prompt, labels, headings, wording, and formatting verbatim.
- Manual compaction and automatic compaction behavior.
- Overflow recovery, cancellation, error behavior, and post-compaction continuation.
- The compacted context rebuilt for the following model request.
- The recorded compaction metadata and `tokensBefore` calculation.
- Usage and cost attributed to the compaction model request.

Do not silently fix, reinterpret, or improve Pi behavior, including observable quirks. Any intentional divergence requires an explicit project decision and documentation; it must not enter as an implementation convenience.

Parity fixtures must feed identical session histories and usage records to pinned Pi and BashKitten and compare the automatic-compaction decision, selected cut point, exact summarization request text and payload, retained context, metadata, and all token calculations. Generated summary prose need not be deterministic, but the request and the way its result is incorporated must match.

## Exact Pi token-usage parity

Rewrite Pi's token-usage and cost-accounting logic in Rust with exact 1:1 parity against the same pinned Pi commit. Pi is the specification for provider usage normalization, context usage, compaction decisions, accumulated totals, cache statistics, and displayed values.

Preserve Pi's exact handling and calculations for:

- Input tokens.
- Output tokens.
- Cache-read tokens.
- Cache-write tokens.
- Total tokens.
- Context tokens, context-window size, and context percentage.
- Per-category cost and total cost.
- Model pricing and request-wide pricing tiers.
- Assistant-message usage, tool-result usage, and compaction usage.
- Cache-hit statistics and resets across compaction.
- Missing, zero, estimated, or provider-specific usage values.
- The period after compaction when context usage is unknown until the next model response.

The agent process is the sole authority for these calculations. Persist the same raw usage fields and calculated values Pi persists. The Web UI must render values supplied by the agent and must not implement a second JavaScript calculation that could drift.

The Web UI must show the actual Pi-equivalent session usage, including current context consumption and percentage, cumulative input and output, cache reads and writes, applicable cache statistics, and total cost. Use Pi's labels, formatting, rounding, and unknown-value behavior verbatim from the pinned version. Never replace an unknown value with a fabricated zero or estimate unless pinned Pi does so.

Parity fixtures must compare BashKitten and pinned Pi across normal turns, tool-heavy turns, cached requests, model changes, compaction, missing provider usage, and pricing tiers. A usage implementation is incomplete if the displayed values happen to look plausible but differ from Pi's calculations.

## Subagents and inter-session communication

A subagent is an ordinary BashKitten session launched by another session. There is no separate subagent runtime or orchestration framework.

Every running session has one Unix-domain control socket and two in-memory message queues:

- `steer`: deliver the message at the next safe agent-loop boundary, before the next model request.
- `queue`: wait until the current work settles, then deliver the message as a new turn.

If the receiving session is idle, either kind of message wakes it immediately.

Use the existing `bash` tool to communicate with another session:

```bash
bashkitten send <session-id> --steer "Check the parser first"
bashkitten send <session-id> --queue "Afterward, inspect the tests"
```

Each session process receives these environment variables:

```text
BASHKITTEN_SESSION_ID=<current-session-id>
BASHKITTEN_PARENT_ID=<parent-session-id>
```

A child can therefore contact its parent while both are working:

```bash
bashkitten send "$BASHKITTEN_PARENT_ID" --steer \
  "I found two implementations. Should I inspect both?"
```

The parent can reply without stopping either session:

```bash
bashkitten send <child-session-id> --steer \
  "Only inspect the implementation under src/core."
```

The same mechanism handles communication from the Web UI, CLI, parent sessions, child sessions, and sibling sessions. Do not introduce a message broker, database-backed queue, dedicated model tool, or separate inter-agent protocol.

The session's socket-listener thread accepts messages while the main thread is streaming a response or running a command. It places each message in the requested in-memory queue. Once delivered to the model, the receiving session adds the message to its in-memory state with the sending session ID and delivery mode. It is persisted at the next normal turn-completion or compaction boundary, for example:

```json
{
  "role": "user",
  "sourceSession": "child-session-id",
  "delivery": "steer",
  "content": "I found a serious parser issue."
}
```

Do not use blocking `wait` as the normal subagent workflow because it can prevent the parent from reacting to a child during the task. Parent and child remain independent active sessions and communicate using `bashkitten send`.

The complete subagent framework consists only of:

- Launching an ordinary session with its parent session ID.
- `bashkitten send --steer`.
- `bashkitten send --queue`.
- The session and parent environment variables.
- One Unix socket and two in-memory queues per running session.
- Markdown guidance explaining this workflow.
