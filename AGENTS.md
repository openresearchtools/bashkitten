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

## Passive Markdown skills

BashKitten uses one deliberately simple, flat skills directory:

```text
~/.config/bashkitten/skills/
├── web-search.md
├── browser-use.md
├── rust-review.md
└── any-name-the-user-adds.md
```

Each skill is an ordinary Markdown file with a short, self-explanatory filename. Do not require a `SKILL.md` filename, one-directory-per-skill layout, front matter, manifests, metadata schemas, a registry, package installation, embeddings, semantic search, or a skill-loader framework. Do not search additional global, project-local, package-provided, or hidden skill locations.

BashKitten itself must not read, parse, preload, rank, select, or inject skill contents into the model context. It only includes brief model-visible guidance that the skills directory exists. The agent decides whether the current task may benefit from a skill, uses the existing `ls` tool to inspect filenames, and uses the existing `read` tool to open only the Markdown files it considers relevant. If no filename appears relevant, it continues without reading a skill.

The default system prompt must communicate this behavior concisely:

```text
Optional skills are ordinary Markdown files in ~/.config/bashkitten/skills/. When a task may benefit from one, use ls to inspect the filenames and read only the files you consider relevant. You may create or edit skill files with the ordinary tools when useful or requested.
```

Skill text enters the model context only as the normal result of the agent's explicit `read` call. A skill is guidance, not executable code, a hidden capability, or an additional tool. Instructions inside a skill may tell the agent to use the seven built-in tools or external commands through `bash`, but BashKitten does not execute a skill automatically.

Users and agents may create, edit, rename, or remove skill Markdown files using the same seven tools and normal filesystem permissions. There is no separate skill-management API or model tool. Agents may also install ordinary system or user packages through `bash` when the operating system permissions and the user's instructions allow it; package management is not a BashKitten capability or dependency resolver.

This passive flat-folder skill design is an intentional BashKitten difference from Pi's skill discovery framework. It must not weaken the exact Pi parity requirements for the seven tool definitions and their tool-specific prompt guidance.

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
- Configuring, authenticating, listing, and selecting models and model-supported thinking levels from the three provider modes defined below.
- Managing the local llama.cpp router and its models through an ordinary settings interface.

The Web UI must bind to `127.0.0.1` by default. Its port comes from BashKitten's local configuration. Changing the port restarts only the Web UI server and must not interrupt agent sessions.

Closing the browser or stopping, restarting, or crashing the Web UI server must not stop any running agent. Agents continue working without a browser or Web UI process.

The Web UI has exactly one local username-and-password account. When no Web UI user exists, it shows first-time signup and allows the first local visitor to choose the username and password. Store only the password hash. After signup, normal visits show the login screen.

Resetting the Web UI user removes the local login identity and invalidates existing Web UI logins. It must not delete session JSONLs, attachments, skills, configuration unrelated to authentication, or running agent processes. The next Web UI visit returns to first-time signup.

The Web UI must not contain a sophisticated file browser, worktree manager, plugin marketplace, MCP interface, npm package system, or unrelated dashboard features. Its provider and model interface must remain limited to the three explicitly supported provider modes below.

## Providers, authentication, and models

BashKitten supports exactly three user-facing provider modes:

1. OpenAI subscription.
2. OpenAI-compatible API.
3. llama.cpp.

Do not add another built-in provider mode. Pin the same upstream Pi commit used for tool, compaction, and token-usage parity. Pi is the behavioral specification for authentication, credential refresh, model definitions, request construction, message conversion, tool calling, image input, reasoning, streaming events, error handling, cancellation, retries, provider usage normalization, and cost accounting for these providers. Rewrite the required implementation in Rust; BashKitten must not require Pi, Node.js, npm, or Pi's JavaScript/TypeScript runtime.

### OpenAI subscription

Port Pi's OpenAI subscription support with exact 1:1 behavioral parity, including:

- The complete OpenAI OAuth login flow and all model-visible and user-visible login behavior.
- Secure local credential storage, credential resolution, expiry handling, refresh, and concurrent-refresh behavior.
- Pi's OpenAI subscription model catalog and model-selection behavior.
- The exact API, request transformations, headers, streaming parser, reasoning handling, tool-call handling, image handling, errors, cancellation, and usage accounting used by pinned Pi.

Do not replace subscription OAuth with an API key, introduce a different OAuth client or flow, or send subscription traffic through the generic provider implementation when pinned Pi uses dedicated OpenAI subscription behavior.

### OpenAI-compatible API

Port Pi's `openai-completions` provider implementation with exact 1:1 behavioral parity. The normal configuration UI must provide:

- A user-defined provider name.
- Base URL.
- Optional API key or bearer token.
- One or more user-configured model presets. Each preset includes its model ID, optional display name, context window, maximum output-token value, supported thinking levels, default thinking level, request parameters, and Pi's applicable compatibility controls and defaults for developer/system roles, reasoning parameters, tool calls, streaming, usage, and other OpenAI-compatible differences.

The UI may discover model IDs from a compatible server when that server exposes a model-list endpoint and offer to create presets from them, but it must always allow the user to enter a model ID directly. A discovered model does not become launchable until the user saves its preset, because its context, thinking, compatibility, and request parameters may not be reliably discoverable. Do not depend on an external model-catalog service. Store credentials locally with access restricted to the current user and never write secrets into session JSONLs, logs, tool output, or browser-visible history.

### llama.cpp

Port Pi's complete llama.cpp router integration with exact 1:1 behavioral parity. This is not merely an arbitrary OpenAI-compatible base-URL preset. Preserve Pi's router connection and authentication, model discovery, persisted model catalog, model loading and unloading, loaded-model selection, Hugging Face search and download flow, quantization selection, cancellation, gated-model warning, retry behavior, and the rule that models are never silently unloaded or deleted.

Use Pi's OpenAI-compatible request path for inference exactly where pinned Pi does. Preserve the provider compatibility settings, model metadata, context-window reporting, streaming, tools, reasoning, image support, usage values, and errors that Pi applies to llama.cpp.

BashKitten additionally owns a small Debian-specific llama.cpp launcher. Detect the installed backend from Debian's package database:

- Installed `llama-cpp-cuda` means the CUDA build.
- Otherwise, installed `llama-cpp` means the CPU/Vulkan build.
- Both packages expose `/usr/bin/llama-server`.
- If neither package or the executable is present, report that llama.cpp is unavailable rather than guessing from GPU hardware or silently installing a package.

Run `llama-server` in router mode without `--model`, `-m`, or `-hf`. Bind it to `127.0.0.1` by default. Its lifecycle must be tracked by the BashKitten systemd user target so a Web UI crash cannot stop it or leave it orphaned.

The Web UI must provide an ordinary llama.cpp settings and model-management interface. Users must not be required to edit a configuration file or type a launch command for normal configuration. At minimum expose:

- Detected package and backend: CUDA or CPU/Vulkan.
- Router running state and restart control.
- Models directory.
- Router port.
- Optional router API key.
- Context size.
- GPU-layer offload as Auto/all available layers, CPU only, or an explicit layer count.
- CPU thread count.
- Batch size.
- Parallel slot count.
- Flash-attention setting when supported by the installed llama.cpp build.
- Memory-map and memory-lock settings.
- Model autoload behavior.
- An optional extra-arguments field for current or advanced `llama-server` options that do not justify permanent BashKitten controls.

Map the normal controls directly to the corresponding `llama-server` arguments. Auto/all GPU offload maps to `-ngl 999`, CPU only maps to `-ngl 0`, and an explicit count maps to that exact `-ngl` value. Enable llama.cpp's Jinja chat-template/tool-calling support by default as Pi requires. Keep all generated launch arguments visible in the UI so configuration is inspectable and hackable.

The llama.cpp page must also provide Pi-equivalent model management: list discovered GGUF models, distinguish loaded and unloaded models, load or unload a selected model, choose whether to retain other loaded models, download by Hugging Face repository and quantization, cancel an active load or download, and select a loaded model for a BashKitten session. Per-model context sizes and advanced overrides must support llama.cpp model presets rather than inventing a second incompatible preset format.

The configured or reported llama.cpp context window must feed the exact Pi token-usage and automatic-compaction calculations. The Web UI must display the value supplied by the agent/provider state and must not independently guess context capacity from the GGUF filename or detected GPU.

### Unified model list, thinking levels, and defaults

The Web UI, BashKitten CLI, user-created sessions, and agent-created sessions must all use one unified model registry and the same validation path. There must not be separate Web UI and CLI model lists that can drift.

Populate the registry as follows:

- After OpenAI subscription login, automatically populate every model available through Pi's pinned OpenAI subscription model catalog. Preserve Pi's model identifiers, capabilities, context values, and supported thinking levels.
- For an OpenAI-compatible provider, include every model preset that the user has explicitly configured in advance. A provider may contain multiple model presets, each with its own parameters and thinking support.
- For llama.cpp, discover GGUF models through Pi's router integration and let the user save the model preset and launch parameters required to run each model. Every configured llama.cpp model must remain visible whether it is currently loaded or unloaded. Unconfigured discovered files may appear in the llama.cpp setup page but are not launchable session models until their preset is saved.

Every launchable registry entry must expose:

- Stable provider and model identifiers.
- Display name.
- Availability and authentication state.
- Context window and maximum output tokens.
- Supported input capabilities.
- Whether reasoning is supported.
- Every thinking level accepted for that model.
- The model's default thinking level.
- The configured parameters needed to invoke or load it.

Never display a thinking level that the selected model cannot actually accept. For OpenAI subscription models, derive thinking support and levels exactly as pinned Pi does. For OpenAI-compatible and llama.cpp models, require the user preset to declare the supported levels and their corresponding Pi-compatible request behavior when the server cannot report them reliably.

Provide the same complete list in the Web UI model selector and through the CLI:

```bash
bashkitten models
bashkitten models --json
```

The human-readable form is for users. The JSON form is the stable machine-readable interface for agents and scripts and must include the allowed thinking levels and default for every model. These are CLI commands invoked through the existing `bash` tool, not additional model tools.

Allow the user to choose one valid default model-and-thinking pair. A new-session form must be prepopulated with that pair while still allowing both fields to be changed before launch. If an explicit model is selected, the thinking selector must immediately show only that model's allowed levels. Starting a session without explicit overrides uses the configured default pair.

Both users and agents can override the defaults when creating a session. The CLI must support:

```bash
bashkitten session start \
  --model <provider/model-id> \
  --thinking <level> \
  --prompt "Inspect the parser"
```

Agent-created sessions use the same command, registry, authentication, validation, and availability checks as user-created sessions. An agent can first run `bashkitten models --json`, choose any listed model and one of that model's listed thinking levels, and launch a child session with that exact pair. Do not restrict agents to the parent's provider, model, or thinking level.

Record the selected provider, model, thinking level, and effective non-secret model parameters in the session's first numbered JSONL so the session can be resumed and audited without `meta.json`. Never persist provider credentials there.

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

The only sidebar-specific sidecar is `title`, a plain text file containing one line. Derive it locally and deterministically from the first user message: use the first non-empty line, collapse whitespace, shorten it to the configured sidebar-title length, and add an ellipsis when truncated. Use a simple local fallback such as `Image session` when the first message contains no text. This display shortening must not alter the complete user message stored in the session or sent to the model. Never make a secondary model request to generate or improve a title. The user or agent may edit the file directly.

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

Before launching a child, an agent may inspect the same model choices shown to the user:

```bash
bashkitten models --json
```

It may then launch the child with any listed model and supported thinking level:

```bash
bashkitten session start \
  --parent "$BASHKITTEN_SESSION_ID" \
  --model <provider/model-id> \
  --thinking <level> \
  --prompt "Investigate the failing tests"
```

If `--model` and `--thinking` are omitted, the child uses the configured default pair. The command prints the new child session ID so the parent can send messages to it immediately.

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

- Listing the shared model registry with `bashkitten models` or `bashkitten models --json`.
- Launching an ordinary session with its parent session ID and optional model and thinking overrides.
- `bashkitten send --steer`.
- `bashkitten send --queue`.
- The session and parent environment variables.
- One Unix socket and two in-memory queues per running session.
- Markdown guidance explaining this workflow.
