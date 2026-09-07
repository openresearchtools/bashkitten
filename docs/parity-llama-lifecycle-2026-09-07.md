# llama.cpp and controller failure boundary verification

Reference: Pi `9841914c71a74d81abe07f751aefd271fd924e63` (verified with
`git rev-parse HEAD` in `/run/media/user/Data/bashkitten-builds/pi-pinned`).

## Decisions recorded before implementation

- Controller settings have no Pi counterpart. As required by BashKitten's
  separate systemd lifecycle, a failed settings apply must restore its prior
  configuration bytes, restart drop-in, login startup setting and Web service
  state. Compensating systemd operations affect only the controller's enablement
  and Web service; they never stop the target, an agent or the router. If rollback
  itself fails, report that failure alongside the original error. This is minimal
  failure handling for the existing settings action, not a new supervisor.
- `AppConfig::save` publishes the llama preset INI before configuration JSON.
  Restore its previous INI if JSON publication fails, avoiding a rejected save
  changing the router's next launch. This is local storage plumbing, not provider
  behavior. Existing successful writes retain the same format.
- Pi `packages/coding-agent/src/extensions/llama/index.ts:66-122` wraps both the
  model load and catalog sync in its restoration handler. Its replacement unload
  loop is deliberately outside that handler; retain this quirk. Pi's
  `ui.ts:493-542` ignores cancellation once the model operation has settled.
  BashKitten will mark the brief catalog/restore phase as finishing under the
  operation mutex, preventing a late HTTP cancel from unloading a completed
  model while still refusing a concurrent operation until catalog sync finishes.
- Pi `index.ts:173-180` passes the catalog returned by `downloadAndWait` directly
  into synchronization. Preserve that catalog, rather than discarding it and
  introducing an extra router list request.

- Pi `huggingface.ts:87-93` applies JavaScript `Number` and truthiness to
  `Retry-After`, then JavaScript string formatting. Rust float parsing differed
  for NaN, hexadecimal/binary/octal, infinity and exponent formatting. Port these
  conversions locally and expand the pinned-Pi offline error fixtures. Token
  trimming uses ECMAScript whitespace from `findHuggingFaceToken` as well.

- Pi `ui.ts:528-537` sends router unload before signalling abort. Match that
  order even when the operation settles while unload is pending, and surface
  an unload failure while still aborting and restoring replacement models.

- The managed router requires a stable configured endpoint; reject port zero,
  which would bind a different ephemeral port that registry clients cannot find.
  Preserve llama.cpp numeric sentinel behavior for other advanced values: the
  installed help documents context zero as model-derived capacity.

## Verification

Completed current-source verification with the existing Podman Rust/Debian build
image and the shared `/build/target` cache:

- `cargo test --locked --test pi_llama`: all three tests pass. The offline
  differential test now executes 76 pinned-Pi router/Hugging Face cases (15 new
  JavaScript numeric conversion error cases). Native INI precedence and actual
  HTTP load polling/cancellation continue to pass.
- `cargo test --locked --lib web::llama_api::tests`: both tests pass, including
  authenticated save of an unloaded local model preset and nine router workflow
  cases: failed replacement load, cancellation with restoration, retain-other
  success, catalog failure after load, late cancellation during catalog sync,
  download cancellation, failure during replacement unload, unload-before-abort
  ordering while unload is blocked, and failed cancellation unload. The same
  HTTP test rejects a simultaneous second operation while sync is pending.
  Normal cancellation has no spurious error; a failed cancellation unload is a
  failed operation with Pi's original router error.
- `cargo test --locked --lib huggingface::tests`: actual loopback HTTP verifies
  search query encoding, repository path encoding, gated repository metadata,
  quantization sizes, malformed successful JSON, private error redaction,
  pre-cancelled requests making no HTTP call, and absent Authorization when an
  optional token is empty.
- `cargo test --locked --lib controller::tests`: both tests pass. Six injected
  systemctl failure/initial-Web-state combinations verify byte-identical
  configuration and drop-in restoration, restoring login enablement, restoring
  active versus inactive Web state, and never touching agent/router/target
  services. A separate case verifies explicit reporting of rollback failure and
  removing a newly introduced drop-in. The GTK button calls this exact native
  handler through the extracted `controller` module.
- `cargo test --locked --lib config::tests::rejected_configuration_publication_restores_previous_llama_preset`:
  config JSON rename is forced to fail after successful INI publication; both
  an existing INI and an originally absent INI are restored correctly.

All network verification here uses loopback fixture servers. Regenerating the
76-case JSON oracle uses the pinned TypeScript code offline; Rust tests and the
installed app do not execute it or require Node.

## Integration boundary

This work does not claim a fresh GTK/installed-package acceptance run. Earlier
real GTK/systemd evidence remains in `tests/live/`; the root agent owns the final
full-suite build, package/install and desktop acceptance. A fresh native
CLI/router/worker check is recorded below.
The new post-save failure path is verified through the exact native handler with
an injected systemctl backend, rather than changing production user services to
induce failure. No installed package or service configuration is modified by
this subtask.

## Independent runtime and storage audit

The follow-up read-only audit identified the following concrete boundaries and
reported them to the root agent, which owns their implementation:

- A queued model-change acknowledgment is not the completed model selection.
  Live/status replay now supplies authoritative model/thinking state, including
  changes made through CLI and failed switches; the UI uses that state.
- Concurrent queue Edit requests could strand an earlier row in an editing
  state. The UI now guards acquisition while its request is in flight. Reload
  and ownership recovery require the separate UI acceptance check owned by the
  root agent; an edit owner is not a login or provider credential.
- Successful acceptance of a Web restart does not guarantee that the new
  process can bind its port. The new Web process now retries the previous port
  from a one-use configuration fallback and atomically clears it. This remains
  within the existing systemd/Web process design.
- Explicit model selection without explicit thinking previously inherited the
  global level even for a preset that only accepts `off`. CLI and Web now share
  the same new-session resolver, which honors the selected preset's declared
  default. Reference: pinned `packages/coding-agent/src/core/sdk.ts:229-253`.
- History readers could observe an incomplete large append. The numbered-file
  implementation now uses an exclusive file lock for each appended batch and
  shared locks for direct segment reads. Older-segment reads and fork source
  reads now also correct sensitive directory/file modes before use.
- Finished-session usage must not lose known context when its preset is removed,
  or inherit the wrong context after a model change. It now falls back to the
  persisted header capacity only when the effective model still matches that
  header's provider/model pair.

The system-prompt/AGENTS precedence audit found no further mismatch against the
pinned system-prompt and resource-loader sources. The usage/cost/cache-checkpoint
audit found no additional mismatch against pinned footer, usage-totals and
`getContextUsage` logic. No extra compatibility-schema restriction was added:
Pi's optional-field schema accepts extra fields, and stricter validation here
would change its behavior.

`cargo test --locked --test session_boundaries` passes all five independent
filesystem tests. Actual `fs2` locks, threads, barriers, partial JSON bytes and
temporary files verify that a reader waits until a partial append is completed,
an append waits for an existing shared reader, older segment reads narrow
permissions, forks narrow the used source files/directories and every output
while copying only retained attachments, and removed-preset usage respects the
matching-model boundary. `cargo clippy --locked --test session_boundaries -- -D
warnings` also passes. These tests make no network or systemd calls.

## Fresh native router and worker acceptance

`tests/live/2026-09-07-llama-router-inference.json` records a successful actual
systemd/router/native-worker run with current debug binaries and the retained
Qwen3 0.6B Q4_K_M GGUF. The launcher generated native INI and ran router mode
without a single-model argument, autoload remained disabled, explicit load
populated the native registry, and a CLI session with omitted thinking used the
saved `off` default. The worker called `write`, produced the independently checked
exact expected file bytes, and completed with a final answer. Its configured
8192-token context appeared in the unified registry.

The model stayed loaded after the turn and after stopping the worker. Explicit
unload succeeded, and the cached model's SHA-256 remained unchanged. Both unique
temporary services are stopped, and production service states and PIDs are
identical before/after. The successful run used no Web process or download;
configuration/runtime/session/work state was isolated under the build directory.
The read-only catalog inspection and explicit model operations contacted only
that test router's loopback endpoint. See the adjacent Markdown evidence file
for the scope and inspection details.
