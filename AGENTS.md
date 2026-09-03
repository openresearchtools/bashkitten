# BashKitten Agent Instructions

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

The session's socket-listener thread accepts messages while the main thread is streaming a response or running a command. It places each message in the requested in-memory queue. Once delivered to the model, the receiving session writes the message to its JSONL with the sending session ID and delivery mode, for example:

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

