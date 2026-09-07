# Pi parity implementation and verification

Reference: Pi `9841914c71a74d81abe07f751aefd271fd924e63`, as recorded in
`PI_UPSTREAM.md`. `AGENTS.md` remains the specification. The current evidence
below supersedes the historical open-item notes in the dated work log.
Only the three required provider modes and seven tools are implemented.

## Current completion record — 2026-09-07

The previously identified implementation gaps have been repaired. This records
specific source, fixture, native-process and browser checks; passing these tests
is not a claim that every possible input or external service state was tested.

| Area | Current implementation and evidence |
| --- | --- |
| Compaction and retry | Seven pinned differential histories plus actual worker threshold/manual/overflow/pre-switch, failed summary, cancellation, atomic rotation/restart and recovery tests. Chronological usage and omitted cache checkpoints now have a separate pinned oracle. |
| Cancellation and tool streaming | Actual provider/socket cancellation, worker stop/SIGTERM and sibling isolation, Pi argument and live bash traces, retained partial output, browser Pause/reload. |
| Seven tools and Unicode | 187 tool calls, five fd 8.6 variants, 24 byte-exact image cases, and 35 Unicode cases. Native UTF-16 preserves lone units through args, errors, edits, truncation, streaming, summaries, JSONLs and attachment forks; provider prose follows Pi sanitization. See `docs/parity-unicode-2026-09-07.md`. |
| Prompt/context | Pinned verbatim prompt/tool fixtures, project-instruction loading, real instruction-only task, request-prefix/restart tests and source review. Passive flat Markdown skills remain the documented difference. |
| OpenAI subscription | Request/catalog, complete Responses signatures/items, SSE/WebSocket continuation, retry/idle/cancellation, cache/affinity, nine pinned refresh outcomes and 116 malformed-JSON cases. Concurrent refresh/logout, failed refresh publication, login lifetime and socket expiry tests pass. Client ID, scopes, endpoints and stored credential format are unchanged. |
| OpenAI-compatible API | Configured presets and Pi request compatibility, reasoning/image transforms, 53 stream cases, 28 partial-JSON cases, SDK HTTP/retry fixtures and browser-to-worker execution. Native UTF-16 and V8 diagnostics retain Pi errors. |
| Usage | Worker-supplied totals/context/cache/costs; Pi footer/decimal fixtures and new checkpoint fixtures. Browser observed exact input/output/cache totals, 32% cache hit rate and 128k→64k model context transition; zero-token aborted response clears latest cache rate as Pi does. |
| llama.cpp | 76 pinned router/Hugging Face cases, actual HTTP search/gated/quantization/redaction checks, nine load/cancel/restore race cases, INI rollback and saved-model validation. Debian launcher and real cached-model acceptance are recorded separately. |
| Live reconnect and queues | Locked JSONL publication/readers, atomic memory subscription, browser reload during live reasoning, held-edit recovery and FIFO execution. Per-edit ownership rejects stale saves/cancels; explicit takeover recovers a lost composer. |
| Settings and models | One native registry/default validation path; explicit models use their supported default. Model changes persist before activation, errors retain prior selection and UI uses authoritative worker state. Config publication restores the preceding llama INI on failure. |
| Sessions, folders and attachments | Exact historical fork bytes and model/folder selection; retained attachments copied privately, including Unicode-only text references. Header-only sidebar and paginated history tests. Old sensitive files are narrowed before use; new files start private. |
| Local Web authentication | OS entropy, hash-only password/session/CSRF storage, one-winner concurrent signup, restart-safe tabs, read-only bootstrap/SSE and Origin/CSRF-protected resume. Logout revokes already-open streams. |
| Lifecycle | Prior real GTK/systemd crash/quit/port/sibling tests plus current exact controller-handler rollback tests. A replacement Web process recovers the old port if the new port becomes occupied after preflight; no extra supervisor is introduced. |
| Delivery | All 131 native tests, strict Clippy, formatting and Debian package checks pass. The .deb is installed system-wide and the existing port-3939 app uses `/usr/bin` binaries. Browser, real cached llama inference and an installed Luna task using all seven tools pass. Temporary services and containers are stopped; old user-local fallback launchers are removed. |

Detailed evidence: `docs/parity-runtime-2026-09-07.md`,
`docs/parity-providers-2026-09-07.md`,
`docs/parity-unicode-2026-09-07.md`,
`docs/parity-llama-lifecycle-2026-09-07.md`, and `tests/live/2026-09-07-*`.
Fixtures use the pinned checkout only during development. The native build,
normal tests and installed application require no Pi, Node.js or npm runtime.

### Authentication diagnosis and successful revalidation

The initial native Codex calls returned `Provided authentication token is
expired.` The previously installed September 5 binaries returned the identical
error with the same saved credential; its stored/JWT expiry values agreed and
were in the future. The comparison preserved the credential bytes and did not
establish the server's reason. No forced refresh or credential import was added.

After the user reported provider login, the installed release successfully ran
`openai-codex/gpt-5.6-luna` through all seven tools. Supplied tests and separate
independent tests passed, supplied tests remained unchanged, no provider errors
occurred, and 6,656 cached-input tokens were reported across seven responses.
See `tests/live/2026-09-07-codex-auth.json` and
`tests/live/2026-09-07-luna-installed.json`. No Astra inference was used.

### Installed release

The queue-composer correction is also installed: saving a held edit takes
precedence over Pause, Steer sends the edited text, and aborted or missing
queue references recover resendable drafts with their attachments. All 134
native tests and strict Clippy pass, with real browser save/steer/stale-edit
recovery checks. See `docs/parity-queue-composer-2026-09-07.md` for evidence and
the latest package hash. The full-context stress test remains ongoing.

The subsequent write-display/folder-button correction is installed in the same
port-3939 service. Completed writes retain their exact contents in an expandable
scroll pane, result status stays on the summary row, and primary dialog actions
retain their light-mode background without hover. All 131 tests passed again;
the live Luna website worker continued through the Web-only restart. See
`docs/parity-write-display-2026-09-07.md` for the current package hash and browser
evidence. The full-context website/automatic-compaction stress test is ongoing.

The final Debian package installs all four native binaries under `/usr/bin`.
The normal `bashkitten-web.service` was restarted on port 3939 with that binary;
provider credential bytes were unchanged by deployment. The old user-local
service/desktop overrides were removed and CLI links now resolve to `/usr/bin`.
Earlier binaries remain only as a rollback backup. At that delivery, only the
normal Web service and shared target were running; the real website test now
also has its normal session service. See `tests/live/2026-09-07-release.json`.

The package was also installed and exercised in a disposable Debian container:
CLI and agent startup, all shared libraries, the eight-model offline catalog,
Web signup/login/CSRF/restart/logout, actual GTK registration and shutdown on a
virtual display, and absence of Node/npm/MTS runtime files. That container has
been removed. GTK's container systemctl calls used a test double; the separate
real systemd/GTK lifecycle evidence remains documented in the dated records.

## Evidence and work log

- Controller lifecycle correction, decision before implementation: the desktop
  entry's native binary delegates to the existing systemd controller service,
  then activates its GTK application over the session bus. The service uses
  Type=dbus and the existing GTK application ID so start waits for the supervised
  primary instance. A private --service argument selects that primary entry path;
  repeated desktop launches present its existing settings window. Quit, close
  and SIGTERM converge on one GTK shutdown handler which stops the shared target.
  This supplies the required controller supervision without adding a supervisor.
- Atomic credential creation correction: serialize and sync to a unique private
  temporary file, then publish signup using a non-overwriting hard link while
  holding the existing process lock. Ordinary replacement keeps atomic rename.
  Unique create-new temporary files also prevent same-process concurrent saves
  from truncating each other's temporary file. Create new private directories
  with mode 0700 from the outset.

- Web authentication correction, mechanism recorded before implementation: GET
  bootstrap must be read-only and opening a second tab must not revoke the first
  tab's CSRF token. Keep each process's OS-random per-login CSRF token only in
  memory. Before listening after a Web restart, issue fresh tokens for surviving
  logins and persist only their hashes; retain hashes issued to still-open tabs
  until that login expires or is invalidated. Signup/login populate the same
  memory map. Authenticated bootstrap only reads it. This preserves hash-only
  credential storage, independent OS entropy, 30-day login lifetime, and no
  persistent browser token storage without adding a CSRF-exempt mutation route.

- Fork attachment correction, source decision before implementation: the required
  independent attachment copies must not rewrite retained message text, tool
  arguments, signatures, or attachment metadata. Preserve non-header JSONL lines
  verbatim. Copy referenced files using their existing relative upload paths,
  including when forking a fork. Resolve a structured attachment to the current
  session's copy only while constructing its provider-visible attachment notice;
  the authenticated download route already resolves under the current session.
  This is the minimal storage adapter required by BashKitten's numbered session
  directories; it does not alter Pi message fields or persisted historical data.

- Continuation 2026-09-05: recovered the last turns of `Fix Pi parity gaps` and
  retained its working tree. The 65 inherited Rust tests pass. The repaired
  project instruction loader passes a real Luna task with an independently
  checked action specified only in AGENTS.md. An actual worker/HTTP test verifies
  that the system prompt, other request fields, and existing message prefix
  remain identical across turns and a restart; prior JSONL bytes remain intact.
- Image implementation source decision before coding: port pinned Pi's
  `mime.ts`, `image-process.ts`, `image-resize-core.ts` and `exif-orientation.ts`.
  Photon already implements decode/resize/encode with native Rust `image` 0.24.9
  (confirmed in its pinned 0.3.4 WASM's embedded dependency paths). Use those same
  native operations directly, preserving its RGBA conversion, Lanczos3 filter,
  encoding order, limits and hints. No WASM, Photon JS or external image process
  is added. Compare actual output bytes and wording against pinned Pi fixtures.

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

## Historical plan from 2026-09-05 (superseded by current completion record)

1. Continue provider transport, timeout, recovery and full tool differential
   fixtures; compaction/retry helpers now have actual worker integration.
2. Port exact system-prompt and project-instruction loading from the verified
   pinned checkout. Finish complete llama.cpp management and provider settings.
3. Exercise all remaining storage/lifecycle/model validation invariants through
   installed processes and browser paths. Verify actual provider behavior before
   certifying a parity row. The separate `pi-pinned` checkout is verified at the
   recorded pin; do not use the older `pi-reference` working checkout as oracle.

## 2026-09-05 runtime compaction and recovery repair

- Verified the separate `pi-pinned` checkout is the recorded pin. Generated
  `tests/fixtures/pi-compaction.json` by executing Pi's actual preparation,
  compaction generator, context builder, token estimator, retry/overflow helpers.
  Fixture setup and its narrow unused transport stub are documented in
  `tests/fixtures/README.md`. Normal Rust tests need no JS runtime.
- Connected manual `/compact` and CLI `session compact`, authenticated HTTP and
  Unix control, automatic threshold/overflow and pre-switch compaction. Manual
  compaction aborts and saves active work first; it does not continue that work.
- Compaction publishes one complete numbered segment atomically. Retained IDs
  and parent links remain intact; `usageBefore` prevents double-counting across
  rotation. Restart rebuilds the summary context from the highest segment.
- Added bounded agent/summary retry using Pi's classifier, per-attempt backoff,
  and cancellation. Failed responses remain in history and leave retry context.
  Overflow has a separate single compact-and-retry limit.
- Differential fixtures exposed JSON object reordering in the value conversion
  path: it changed serialized tool arguments in summary prompts and persisted
  replay. Enabled `serde_json/preserve_order`; pinned-Pi prompt fixtures pass.
- Added actual worker/socket/HTTP fixtures for transient retries, compaction and
  resume, overflow recovery, manual abort-save-compaction, failed summary leaving
  the old segment/model untouched, and pre-switch summarization using the old
  model. These supplement the earlier partial-output and sibling-worker tests.
- Removed an unconnected, unverified generic HTTP idle-timeout setting. Pi's
  provider-specific timeout rules still need porting at their actual call sites.
- This is an implementation checkpoint, not complete parity or a deployed build.

- Usage display now comes from one Rust agent-library snapshot. Live workers
  publish it and include it in atomic subscriptions/status; the Web server uses
  the same restoration routine for finished current segments. No JavaScript
  totals, pricing fallback, context percentage, or cache-hit calculation remains.
  Fourteen fixtures execute the actual pinned Pi FooterComponent with fixture
  contexts and compare its rendered statistics; token and Number.toFixed boundary
  fixtures cover JavaScript decimal rounding. Pre-switch checks use pure active
  message estimates when pre-compaction provider usage is stale.

- Queue editing requires one local hold flag on an existing queue entry (Pi has
  no Web composer edit lifecycle). This is the smallest mechanism for the
  required edit-with-position-and-attachments behavior: acquire under the queue
  lock before loading the composer, release on save/cancel, and never allow a
  later follow-up to pass the held FIFO head. The flag is not a new agent tool.

- Browser fixture: 105 private test chats verified sidebar page continuation;
  opening the three-segment chat rendered only its newest 20 messages, then one
  upward scroll prepended 20 older messages in order while preserving the viewport.
  Opening now obtains the newest segment directly rather than trusting sidebar
  metadata. Async history/status results are scoped to each chat opening.
- Prompt port decision (before implementation): preserve the pinned default
  prompt wording, including its Pi documentation references. Ship those pinned
  documentation/examples as inert local reference data, with their MIT notice;
  no Pi executable, dependencies, loaders, extensions or runtime are installed.
  Absolute docs paths map to `/usr/share/doc/bashkitten/pi-reference/` under the
  Debian layout. Pi's global agent directory maps to BashKitten's config directory
  and project `.pi/SYSTEM.md`/`.pi/APPEND_SYSTEM.md` keep Pi's exact project paths under the
  deliberately selected working folder. Folder selection authorizes that folder's
  project instructions; no second trust/permissions interface is added. Replace
  Pi's active skill injection only with the exact passive-skills paragraph from
  AGENTS.md. Cache the constructed prompt for the worker lifetime and rebuild at
  a completed-turn folder change, preserving a stable request prefix across turns.

- User-reported sidebar regression fixed: constrain the app grid row and sidebar
  flex children to the viewport, keep the 58px header outside the scrolling list,
  and preserve list scrollTop across periodic rendering. Browser measurement with
  106 chats: header top 0, root scrollTop 0, sidebar scrollTop 1698, transcript
  scrollTop unchanged at 2658. Browser and transcript scroll independently.
- User-reported OAuth browser launch regression: removed blank popup followed by
  async navigation. Ported pinned `utils/open-browser.ts` / `LoginDialog.showAuth`
  Linux behavior: detached `xdg-open` with the complete authorization URL passed
  as a single argument, no shell, ignored stdio, best-effort launch. The user
  explicitly requested no visible authorization link, so that link is removed;
  the required manual code/redirect input remains. `xdg-utils` is a Debian package
  dependency. Native launcher argument test and actual desktop launch pending.

- Long storage paths exposed a Linux AF_UNIX pathname limit in the isolated
  process test. The failed socket-listener task was also hidden behind a timeout.
  Architecture plumbing decision before the repair: preserve the mandated
  `sessions/<id>/control.sock` filesystem location, use an open parent-directory
  descriptor via `/proc/self/fd/<fd>/control.sock` only when the address exceeds
  Linux's socket pathname limit, and retain that descriptor through bind/connect.
  Bind synchronously before publishing a worker, propagate its actual failure,
  and refuse to remove another live worker's socket. This adds no protocol,
  socket location, index, supervisor, network destination, or model tool.

## 2026-09-05 tool, image, instruction and credential continuation

- 92 calls against the actual pinned seven Pi tools now match committed Rust
  fixtures, including coercion, validation order, path fallbacks, cancellation,
  errors, fractional/negative limits, edit patches and truncation. The native
  edit patch generator ports locked jsdiff 8.0.4 (MIT notice retained).
- 24 pinned Photon image cases match exact encoded output byte hashes, metadata
  and wording. This includes resize, PNG/JPEG fallback, BMP conversion, invalid
  files and all eight EXIF orientations. Tool reads and attachment ingestion use
  the same native processing; persisted images survive a text-only to vision
  model switch without being processed again.
- Full Rust suite at this checkpoint: 64 unit tests and five integration test
  functions passed; strict all-target clippy passed. The subsequent authenticated
  HTTP credential test also passed: settings never return stored secrets,
  missing values preserve existing credentials, explicit values replace/clear.
- Two newly built real workers completed independent Luna inventory repair and
  Sol parser refactoring tasks, including the supplied and independent tests.
  Evidence is in tests/live/2026-09-05-tool-repair.json and its Markdown report.
- The real instruction-loading test passed again after a worker restart. The
  provider returned 1,536 cached input tokens during that restarted turn.
  Deterministic worker tests also compare the complete unchanged request prefix.
- Full tool parity is not yet certified: isolated UTF-16 surrogate truncation,
  further fractional-index/error boundaries and platform/locale behavior remain
  unverified. Provider transport and the remaining ledger gates remain open.

## llama.cpp architecture decisions before the next port

- Use pinned extensions/llama/client.ts, huggingface.ts, provider.ts and index.ts
  for router requests, SSE/polling, cancellation, errors, catalog metadata and HF
  actions. Router discovery is /models, not the obsolete /v1/models helper.
- The required Web settings need one in-memory operation record for progress and
  cancellation; it belongs to the UI server, not an inference runtime or process
  supervisor. Systemd continues to own the independent external router process.
  Read-only status never initiates HF searches/downloads. Authenticated explicit
  requests authorize router/HF actions; every outbound call documents its scope.
- Debian package detection and launch argument construction are shared by CLI,
  Web and workers. Displayed arguments redact credentials. Router arguments may
  never include a single-model launch switch. Stored catalog metadata supplements
  explicit saved model presets; unconfigured discoveries never become launchable.

- llama.cpp continuation: 61 pinned Pi router/HF fixtures pass, plus actual HTTP
  polling/cancellation tests. The wrong unused /v1/models discovery implementation
  and its misleading test were removed. Shared Debian package detection, launch
  arguments, native router/HF clients, authenticated settings controls and model
  operations are connected. Catalog data persists and feeds the shared registry.
- Full suite after this integration: 66 unit + seven integration test functions
  passed (73 total). Strict clippy found six mechanical nested-if warnings; its
  native fixes were applied successfully. Further workflow tests are being added.
- Installed router test used Debian llama-cpp-cuda build 10545 / a30273376 in an
  isolated systemd user service and empty private models directory. Actual /models
  returned the expected router catalog. Browser refresh discovered an intentionally
  invalid GGUF; save persisted GPU count 17, rendered `-ngl 17`, and registered an
  unavailable preset visible in both Web and CLI. Loading failed with the actual
  router's `Model exited with code 1`; the source GGUF remained intact. No external
  model download, existing router restart or existing session interruption occurred.
- A remaining preset issue is now confirmed in the installed llama.cpp source:
  CLI options override per-model INI values. Merely saving a context field in the
  BashKitten registry does not configure the router. Required next implementation:
  preserve llama.cpp's native INI format and route shared model defaults through
  its global section when per-model overrides are present; display the generated
  CLI and INI together. See the installed-version source/docs at
  https://github.com/ggml-org/llama.cpp/blob/a30273376/tools/server/README.md#model-presets
  and server-models.cpp (overlay of base_preset). No separate preset format or
  invented precedence is permitted. This row remains incomplete until tested.

- Native INI precedence repair is now connected. Normal per-model context and
  advanced INI entries generate a private llama-models.ini using native global
  and named sections. The managed launcher keeps router switches on the command
  line and moves model defaults into the global INI section when presets exist.
  Explicit advanced CLI options retain llama.cpp's documented highest precedence.
  No loaded model is silently restarted when its preset changes.
- Verified against installed a30273376: saved global context 32768 / GPU 17 and
  model context 4096 / GPU 9 / batch 64 through the actual browser. After an
  explicit restart of only the isolated router service, its /models response
  reported child arguments --ctx-size 4096, --n-gpu-layers 9, --batch-size 64.
  The router remained PID 3266835 during the earlier isolated Web restart;
  Web login also survived that restart. These checks used a deliberately invalid
  fixture GGUF, not successful local-model inference.
- Authenticated HTTP workflow tests now cover load failure, cancellation,
  restoration of replaced models, keeping already-loaded models, catalog
  persistence and credential redaction. Duplicate Origin headers are rejected.

- Shared launch/switch validation now rejects unavailable registry entries and
  unsupported thinking levels consistently in Web, CLI and workers. A worker
  reloads the saved registry at a completed-turn switch while retaining the old
  endpoint for any required pre-switch compaction. The actual worker/socket/HTTP
  test for a preset added after worker startup passes.
- OpenAI-compatible continuation: captured 139 offline pinned-Pi request cases
  in pi-completions.json with the actual buildParams/getCompat algorithms. They
  cover automatic compatibility detection, all thinking formats and null maps,
  budgets, cache control, role/message transformations, image downgrade, orphan
  results, tool IDs, explicit overrides and context-clamped output limits.
  These are oracle expectations, not passing Rust evidence yet. Next port must
  preserve full logical message metadata before conversion: the current flattened
  ProviderMessage loses source model/API identity and cannot reproduce Pi's
  cross-model signature and tool-ID transformations.

- Compatible-provider repair: replaced the old approximate request builder with
  a native port of pinned `openai-completions.ts`, `transform-messages.ts`,
  `simple-options.ts` and the request context estimator. 139 executable Pi cases
  pass. Full logical source identity, signatures, usage and timestamps now reach
  conversion. BashKitten attachment references are converted to ordinary text
  first; a real integration regression exposed and fixed empty image URLs.
- 46 actual Pi SSE cases now pass through Rust HTTP and response assembly:
  finish errors, optional finish reasons, first response ID/model, distinct tools
  without stream indexes, late identities, partial argument JSON, reasoning
  field precedence, structured/encrypted reasoning replay and cache fallbacks.
  25 pinned partial-json/repair cases pass. Enabled serde_json float_roundtrip so
  parsed provider numbers preserve the binary values used by Pi's cost math.
  Additional transport, abort and malformed-input boundaries remain open.

- Settings plumbing: advanced llama.cpp arguments can also contain credentials.
  Return opaque placeholders for API/HF token arguments and restore existing
  values in Rust only for the same option occurrence when saving. This extends
  the required credential protection without a new secret-storage API.

- HTTP transport decision: preserve Pi's user agent, API headers, session
  affinity, option precedence and 10-minute response-header timeout. Its SDK
  `x-stainless-*` runtime/language/package reporting is omitted under the explicit
  no-telemetry rule; no replacement BashKitten attribution is added. Optional
  compatible-provider authentication remains optional as AGENTS.md requires.
  Port native error-body normalization and bounded provider retry from the same
  pin; never replace quota errors with generic rate-limit wording. Known/local
  token values and token-bearing JSON fields are redacted before publication.

- 25 actual Pi SDK HTTP/retry/header fixtures pass in native HTTP tests, including
  429 quota exhaustion, retry-header precedence, attempt count, delay caps,
  option-header precedence and unshortened affinity IDs. Separate native tests
  verify the header timeout, cancellation during retry backoff and redaction.
- Fixed output-limit continuation from pinned agent-loop.ts: fail every tool
  call in the truncated assistant message with Pi's exact error and details {},
  then continue so the model can re-issue it. The actual Pi loop oracle performs
  three requests and only one tool operation; the Rust worker now follows that
  sequence and preserves the same logical messages and filesystem effects.

- Compatible settings now expose provider names, authentication, multiple model
  presets, explicit thinking-level declarations/defaults, common Pi compatibility
  controls, all additional compatibility JSON, request parameters, budgets/maps
  and cost rates. One Rust preset validator is used for saved configuration and
  the llama preset endpoint. Registry JSON now includes authentication status
  and non-secret invocation/loading parameters for both Web and CLI consumers.
- Isolated browser on port 49713 saved and reloaded a 65536/4096 reasoning model
  with off/low/high (default low), Qwen template and advanced compatibility, plus
  a separate text-only preset. The chat selector displayed exactly off/low/high;
  CLI JSON showed both saved presets. No browser console errors were reported.
  This verifies the editor path; further malformed settings and actual configured
  inference checks remain open.

- The required first JSONL header now records the registry's effective non-secret
  modelParameters for Web and CLI session creation. Later segment rotation uses
  the currently selected model's parameters. Per-change audit/replay of edits to
  the same preset still needs further work; this does not certify that row.
- Completed the configured compatible-preset browser-to-worker test with a local
  deterministic HTTP server: expected auth and affinity, Qwen template, top_p,
  output limit, exact seven tools, actual proof-file write, final transcript and
  collapsed work trace all passed. Saved evidence is
  tests/live/2026-09-05-compatible-settings.json. Its worker/Web/fixture services
  were stopped; production services remained untouched.

- Web-port lifecycle decision before implementation: capture the server's actual
  bound origin for CSRF until that process stops. On a validated port change,
  check the destination loopback port, find the current Web process's own systemd
  service from its cgroup and verify MainPID, save configuration, then ask systemd
  to restart only that service after returning the HTTP response. Never guess a
  service name or restart a parent application's unit. No second supervisor or
  agent-process ownership is introduced; unsupervised development servers report
  that an own systemd service is required for this lifecycle operation.

- Actual systemd port-change test passed with Web PID 3463467 -> 3469926,
  agent PID 3469887 unchanged while bash slept, then verified proof file and
  persisted final answer. Browser reopened the complete collapsed trace. Login
  survived, an occupied port was rejected before saving, and initial header
  modelParameters matched the saved preset. Evidence: tests/live/2026-09-05-web-port-lifecycle.json.
  Isolated Web/fixture services were stopped; production remained untouched.
- Codex request port decision: reuse the already-fixtured transform-messages
  algorithm with Pi Responses tool-ID normalization and consume the complete
  logical messages. Preserve all reasoning JSON, text signatures/phases, source
  model/API comparisons, orphan results, tool-output image blocks, exact prompt
  whitespace, and dedicated Codex options. No grammar/deferred tool surface is
  added: BashKitten has only the seven specified JSON-schema tools.

- Settings organization and local model discovery decision (explicit user request,
  2026-09-05): use four top tabs, App / Subscriptions / APIs / llama.cpp. Keep
  the existing provider modes and place all llama model listing/download controls
  under its tab. Reuse the existing folder-only picker for configured model roots.
  Local GGUF discovery in Hugging Face caches and typical LM Studio folders is
  read-only and performs no network requests, copies, downloads or model loads.
  Discovered files become launchable only after saving their native llama preset.
  Optional CUDA/Vulkan visibility environment overrides are passed only to the
  separate systemd llama-server service. Backend details and source citations are
  recorded in docs/settings-local-models.md. These are the narrow user-authorized
  additions to the pinned Pi router workflow, not a new provider or tool surface.

- Replaced approximate Codex request conversion with complete logical Responses
  replay. 160 actual pinned request fixtures pass, including serialized key order.
  The shared catalog now embeds Pi's eight final explicit model definitions,
  costs, capabilities and thinking mappings. Running the complete generator
  metadata passes revealed that its final Codex override restores Astra minimal
  as an alias for low; the previous hand-written registry incorrectly hid it.
  The fixture preserves that Pi quirk. Codex transport/stream differential
  repair is still open.

- Dedicated Codex SSE HTTP transport now passes 26 actual pinned cases through
  native HTTP, including zstd level-3 request compression, exact URL handling,
  retry quirks, usage-limit wording, retry-delay bounds and clamped session/cache
  affinity headers. Provider error tokens are redacted before publication.
  WebSocket auto/cached transport and full streaming parity remain open.
- Settings/local-model addition verified end to end in an isolated browser:
  the four top tabs show only their own components; HF cache GGUFs and a custom
  picker-selected folder appear in local listings; the picker retains parent
  navigation, absolute entry and folder creation. Saving a selected local file
  produces a native `model = /absolute/path.gguf` INI entry and the real router
  lists that saved preset as unloaded. CUDA/Vulkan overrides survived Web restart
  and were read back from the separate native llama-server process environment.
  Eight focused native tests, three Pi llama integration tests, binary build,
  Clippy and browser console/layout checks passed. Evidence:
  tests/live/2026-09-05-settings-tabs-local-models.json. The test used invalid GGUF
  fixtures and makes no successful-inference claim. Its Web/router services and
  browser tab are closed; production processes were untouched.

- Complete Codex SSE logical-state comparison now passes 66 actual pinned cases:
  block creation order, full item endings/backfill, response metadata, terminal
  status quirks, unfinished argument scratch state, custom-input failures and
  usage/tier costs. This is not yet a certification of every malformed stream.
- Codex WebSocket implementation decision before coding: port pinned
  openai-codex-responses.ts acquisition, per-session/account connection cache,
  full/delta continuation matching, 5-minute idle / 55-minute age limits,
  first-event start, timeout/cancellation, single retry for missing continuation
  or pre-start connection limits, and session-latched SSE fallback. Use native
  Rust WebSocket/TLS; no runtime JS. Diagnostic logical fields are retained;
  stack traces necessarily refer to the native implementation, not fictitious
  JavaScript frames. Native socket errors must retain their applicable details.

- Native Codex WebSocket port now passes 19 actual pinned full transport
  scenarios plus 11 exact serialized continuation cases: cached/full context,
  unset/explicit transport distinctions, no-cache connections, before/after
  start API/transport failures, single retries, idle timeouts and sticky SSE
  fallback. Five URL cases pass; native headers use the pinned handshake fields.
  Socket expiry/concurrency and malformed-JSON wording
  and runtime/platform network-error variants still need boundary work.
- New real Luna and Sol coding runs both passed supplied and independent tests,
  retained the original tests, and persisted no provider failures or transport
  fallback diagnostics. Both used configured auto transport; provider-reported
  cached input was 9,216 and 8,576 tokens respectively. Luna used all seven tools.
  Evidence: tests/live/2026-09-05-codex-transports.json and its Markdown companion.
  Both isolated agent processes completed; production services were untouched.

- The Codex stream oracle now has 100 cases, including requested-vs-reported
  service-tier precedence and priority pricing across model IDs. Native request
  options carry service tier, verbosity and reasoning-summary fields. All pass.
  Cancellation tests prove handshake EOF, active-socket close, and a fresh
  same-session request after cancellation. Provider cancellation tracks transport-specific Pi wording. Known provider tokens are redacted on streamed
  failure paths as well as HTTP failures. A malformed later SSE frame no longer
  discards the valid events that preceded it in the same network chunk; exact
  V8 malformed-JSON wording remains explicitly open.

- Current complete Rust suite passes after the Codex transport integration. The
  ordinary binaries build and Clippy passes; subsequent focused additions cover
  tier precedence, cancellation and malformed-frame retention. Full parity and
  installed delivery remain active requirements, not completed by this checkpoint.

- Pinned SDK HTTP-dispatcher source revealed the missing 300,000ms worker idle
  default. Both HTTP provider bodies now enforce the configured idle interval;
  the same effective timeout reaches Codex WebSocket reads and provider header
  waits. App settings expose Pi's 30-second, 1-, 2-, 5-minute and disabled choices.
  Four actual pinned loopback HTTP cases establish idle failure (`terminated`),
  partial preservation, and different abort wording: Codex HTTP body waits use
  `This operation was aborted`; compatible streams use `Request was aborted`.
  The response assembly now carries the active transport's abort message.
- Real llama.cpp download/load/inference follow-up passed through authenticated
  BashKitten endpoints: unsloth/Qwen3-0.6B-GGUF Q4_K_M (396,705,472 bytes) downloaded
  with live progress, saved as an 8192-context/1024-output preset, and loaded in
  the separate native router with GPU layers 0 and four CPU threads. One session
  wrote an exact proof file; an independent fresh session invoked `read` and
  returned its content, with host-side byte verification. Web restart retained
  the loaded model. The tiny model's earlier unwanted extra write and its later
  answer-from-history without a requested read are preserved as failed model
  instruction-following cases, not hidden. Evidence and full logical messages:
  tests/live/2026-09-05-llama-download-inference.json. Browser surfaces were
  unavailable for this follow-up, so it exercised the same authenticated APIs
  behind the separately browser-tested UI. No code correction was required.
  Test services/workers are stopped; the GGUF remains in the isolated
  /run/media/user/Data/bashkitten-builds/llama-live-download/models cache.

- User-directed exception (2026-09-05): omit proxy support. All BashKitten-owned
  provider, OAuth and Hugging Face HTTP clients explicitly disable environment
  proxy discovery. API presets accept either HTTP or HTTPS and connect directly
  to the configured URL, including local vLLM at http://127.0.0.1:8000/v1.
  Local llama.cpp uses loopback HTTP; OpenAI subscription uses its dedicated
  HTTPS/WSS endpoints. Codex WebSockets also connect directly. Removed the
  in-progress proxy port, its extra dependencies and fixtures. This overrides Pi's optional proxy
  behavior; it does not add a fourth provider or any external service.

- Session/model audit before correction: a folder change must replace only the
  current header's folder fields. The live model may differ from the segment's
  initial model; copying the entire live header would corrupt the baseline for
  historical forks. Preserve that baseline and replay Pi's model/thinking
  entries. Pinned `AgentSession.setModel` always appends `model_change`, including
  reselecting the same ID, while `setThinkingLevel` records only actual changes.
  Validate the configured default pair against the same registry used at launch;
  temporary missing authentication or an unloaded router model does not make a
  registered default pair invalid. Actual launch still checks availability.

- Explicit folder-picker correction (2026-09-05): chat working folder, App
  default working folder, llama.cpp models directory and additional model folders
  must open the same folder-only dialog. An initial configured folder may not
  exist yet; start that dialog at its nearest readable parent so navigation and
  creating the missing folder work. This fallback does not save or select a
  different configuration automatically. Explicit Go/selection remains strict.
  Ignore responses from a previously closed or superseded picker request.

- Verification: 76 library tests now pass, including same-model selection,
  model/folder historical forks, shared default-pair validation and nearest-parent
  folder opening. The API HTTP fixture also passes with every proxy environment
  variable deliberately set to an unusable server. Real browser checks saved
  App default folder, llama.cpp models directory and extra folders through the
  one shared dialog, exercising create, parent, absolute path and missing-folder
  recovery. Evidence: tests/live/2026-09-05-shared-folder-picker.json.
- Running UI correction: the installed Web process on port 3939 and the older
  debug preview on port 18761 were still serving pre-tabs HTML. After browser
  verification, copied the current four native binaries to
  /home/user/.local/lib/bashkitten/current and added user-service ExecStart
  overrides for Web, controller and llama.cpp. Only Web was restarted
  (1695844 -> 3747972); controller/router were stopped and no agent sessions were
  running. Web starts new agents from the matching user-local binary. The old
  system package and old preview on 18761 were not modified; system package
  installation requires administrator authentication. Port 3939 now serves all
  four shared-picker entry points. Isolated picker test processes are stopped.

- Browser queue/reconnect/fork verification (2026-09-05): held FIFO edits retain
  attachments; edit, cancel, removal and steering promotion deliver in the
  expected order. A second tab restores thinking, active bash output and partial
  text without duplicate messages. Pause persists the partial assistant and
  its abort result. Fixed streamed thinking moving below its completed tool.
  The image picker, composer/transcript thumbnails and chat-pane viewer passed.
- Forking no longer performs recursive string replacement over history. Browser
  forks preserve all five retained non-header lines byte for byte, copy only the
  referenced text attachment and exclude a later image. Anonymous downloads
  return 401; authenticated downloads return the exact copied bytes. Native
  tests additionally exercise nested forks, quoted filenames and a resumed
  worker using its own copy after the source attachment is removed.
- Bootstrap no longer rotates CSRF state on GET. OS-random tokens are held in
  Web process memory and only hashes survive on disk; prior hashes remain valid
  for the same login lifetime so already-open tabs survive restart. Eight
  simultaneous HTTP bootstrap requests leave the auth file unchanged. Browser
  tabs continue after Web restart and second-tab reload. Invalid Origin/token
  requests remain rejected before mutation; existing broad auth-file permissions
  are corrected before reading. The frontend recovers only an explicit CSRF
  rejection (which cannot have performed a mutation), covering stale pages.
- Verification: 101 Rust tests, including 79 library tests, pass; Clippy with
  warnings denied and embedded JavaScript syntax pass. Evidence is recorded in
  tests/live/2026-09-05-queue-reconnect-fork.json. Updated the four user-local
  native binaries and restarted only port-3939 Web (3747972 -> 3853911); no agent
  services were running. Isolated test Web/provider processes and browser tabs
  are closed. The system package and old port-18761 preview remain unchanged.

- GTK settings follow-up: resolve the Web service canonical unit ID for its
  restart drop-in, respect the XDG user configuration directory, reject an
  occupied destination port before saving, and display save/systemd failures
  instead of silently discarding them. No additional controller controls added.

- Installation consistency correction: session launch resolves the agent binary
  beside the launching native executable (while retaining the explicit override)
  and prepends that installation directory to the worker PATH. This prevents a
  current user-local agent from invoking an older system CLI/model registry via
  bash, and prevents the current CLI from launching the old system agent.

- Controller/runtime validation (2026-09-05): ran the real GTK binary on a private
  Broadway display with real systemd units under a test prefix, a real Debian
  llama-server and a streaming fixture agent. Desktop-style invocation delegates
  to Type=dbus; repeated invocation reuses the primary window/PID. Killing the
  controller restarted it (3888406 -> 3890665) without changing Web/router/agent
  PIDs. GTK Quit, SIGTERM and normal window close each stopped the entire test
  target and persisted the active partial assistant with stopReason=aborted.
- GTK controls: occupied-port rejection preserves config bytes. A successful
  port change restarts only Web. Start-at-login enables/disables the controller
  unit; restart-off keeps a crashed Web stopped and restart-on restarts it while
  preserving siblings. Open Web UI explicitly starts a stopped Web and invokes
  the correct loopback URL (browser command captured by the test harness).
- Private storage: eight concurrent signups yield exactly one complete Argon2id
  identity. Create-if-absent refuses both existing files and dangling symlinks;
  eight same-process atomic saves yield one complete 0600 file without leftover
  temporary files. New private directories are created as 0700.
- Installation consistency: an actual CLI-created agent, without an agent-binary
  override, runs the sibling current agent. Its bash tool resolves the sibling
  current CLI and returns the shared fixture plus pinned Astra model catalog.
  All 103 Rust tests (81 library tests) and Clippy with warnings denied pass.
  Evidence: tests/live/2026-09-05-controller-lifecycle.json.
- Updated the user-local four-binary installation, controller service override
  (Type=dbus, BusName, --service), user desktop entry and user CLI/controller
  symlinks. Port-3939 Web restarted alone (3853911 -> 3955580); no agent sessions
  were running. Production controller remains stopped until opened. All test
  units, test-only restart drop-ins, fixture provider/display and browser tab are
  removed or stopped. The system Debian package and old port-18761 preview are
  unchanged; package and final Git delivery remain open.

- Tool boundary continuation (2026-09-05): 146 actual pinned Pi calls pass,
  expanded from 92. Corrected fractional read slice coercion (including negative
  fractions), Pi's fractional oversized-line error, JavaScript number rendering,
  exact whitespace conversion, and binary/octal/hex conversion beyond 64 bits
  with one correctly rounded binary64 result. Raw-JSON fixtures verify numeric
  spelling, precision and integer-key ordering in validation diagnostics.
- Replaced the partial hand-written edit normalization with full Unicode NFKC
  and JavaScript trimEnd semantics. Pinned calls cover circled/roman/superscript
  symbols, combining accents, Hangul, ligatures, supplementary mathematical
  letters, trailing BOM, and U+0085 preservation, including resulting file bytes.
- find now passes Pi's numeric limit string to fd and returns fd's own errors.
  The oracle's cached fd is 10.5.0 (host fdfind is 10.3.0); Debian Bookworm ships
  fdfind 8.6.0, whose clap diagnostics differ. Five additional actual Pi runs
  with the Bookworm binary are stored in tests/fixtures/pi-tools-fd8.json.
  The native test selects that exact installed-version fixture, with no error
  normalization. Generate it with PI_TOOL_CASE_PREFIX=find-boundary and a
  private PI_CODING_AGENT_DIR containing the desired bin/fd. No user Pi cache
  is modified, and oracle generation remains offline.
- All 103 Rust tests pass. Isolated UTF-16 surrogate strings, additional
  filesystem/error boundaries and the other ledger gates remain open.
- Delivery checkpoint: all-target Clippy with warnings denied passes. The
  tool fixtures also pass on host fdfind 10.3.0. Built the optimized release
  Debian package (12,182,512 bytes; SHA-256
  454b7992455acbbfabea13051ae34d6cff7dbbe0d1b230bce300af87490f9d4b).
  An isolated Debian install passed dpkg status, all four ELF dependency checks,
  the eight-model CLI catalog, packaged controller unit, embedded settings and
  shared picker checks, initial bootstrap and anonymous model endpoint rejection.
  Node.js was absent; the packaged Web process was stopped and the container
  removed. Evidence: tests/live/2026-09-05-tool-boundaries-package.json.
- Deployed the four optimized release binaries to the existing user-local
  installation and restarted only Web (3955580 -> 4079364). Port 3939 serves the
  shared picker; controller/router remain stopped and no agent services were
  running. The host system package and old port-18761 preview remain unchanged.

- Tool filesystem/streaming continuation (2026-09-05): the actual pinned tool
  corpus grew to 187 cases. grep now falls back to lossy UTF-8 file decoding
  when ripgrep emits bytes, preserves fractional context behavior, and counts
  matches even when a non-UTF-8 filename cannot be formatted, as Pi does.
  Raw file-name and file-byte fixtures verify these outcomes.
- write/edit mutation registration now resolves the complete path before the
  first cancellation check, falls back only for ENOENT/ENOTDIR, and propagates
  realpath failures. Removed the extra canonical-parent fallback absent in Pi.
  NUL paths fail during registration even for already-cancelled calls. Fixtures
  verify symlink loops, missing parents, directory reads, and filesystem effects.
  edit now performs access, read and write in Pi's sequence instead of opening
  a read/write descriptor before the read. Argument diagnostics reproduce the
  pinned oracle's Node 22.22.1 escaping and quote selection.
- bash now emits the initial update before timeout/abort validation, omits
  undefined metadata fields, flushes dirty output after Pi's 100 ms throttle,
  and avoids duplicate final updates. One event loop maintains updates, timeout
  and cancellation during the 100 ms post-shell-exit idle grace. Actual Pi
  update traces cover empty/short/burst output, actively writing descendants,
  cancellation after shell exit and timeout with inherited pipes still open.
- Confirmed open UTF-16 gate: tests/fixtures/pi-surrogate-wire.json captures
  paired and split-surrogate grep output, exact raw tool JSON and actual Pi
  Responses conversion. Pi preserves the unpaired code unit in logical history
  and strips it at provider conversion; the current Rust String replacement
  loses that distinction. These two cases are explicitly excluded from the
  passing count until lossless storage, live serialization and provider replay
  are repaired together. The requirement is not reduced to the provider output.
- Verification and delivery: all 103 Rust tests, strict all-target Clippy, and
  host fd tool comparisons pass. The new optimized .deb is 12,184,264 bytes,
  SHA-256 2de74b5448a2336ad065dfc2b58da53106ac7ba0c81ed61aa2f1de47e1bb2058.
  An isolated Debian installation again passed the CLI/catalog, linked-library,
  packaged Web/settings/picker and anonymous-auth checks without Node.js.
  Evidence: tests/live/2026-09-05-tool-streaming-filesystem.json.
- Deployed all four matching release binaries to the existing user-local
  installation; only Web restarted (4079364 -> 114039). No agent services were
  running; controller/router remain stopped. Port 3939 serves the current
  embedded UI. The isolated package container and Web process are stopped.
  Host system-package installation and the other completion gates remain open.
