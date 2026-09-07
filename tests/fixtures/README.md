# Pinned Pi differential fixtures

`pi-compaction.json` is produced by **unmodified algorithm source** from Pi
`9841914c71a74d81abe07f751aefd271fd924e63`. It contains exact prepared histories,
cut points, context, estimates, requests, deterministic fake summary results,
combined summary usage, file-operation tracking, and recovery classifications.
The generator refuses a checkout with a different HEAD.

To regenerate (development only):

```sh
export PI_REFERENCE=/run/media/user/Data/bashkitten-builds/pi-pinned
# Once, in that separate Pi checkout:
(cd "$PI_REFERENCE" && npm ci --ignore-scripts --no-audit --no-fund)
python3 scripts/prepare-pi-fixtures.py
PI_OFFLINE=1 "$PI_REFERENCE/node_modules/.bin/tsx" \
  --tsconfig "$PI_REFERENCE/tsconfig.fixture.json" \
  scripts/generate-pi-compaction-fixtures.mts tests/fixtures/pi-compaction.json
cargo test --locked --test pi_compaction
```

Pi's compatibility import eagerly loads generated external model catalogs.
The preparation script replaces only the unused `completeSimple()` fallback
with a throwing stub in a separate development tsconfig. The tested `compact()`
uses its upstream `streamFn` injection to capture requests and supply a fixed
response. No provider request, catalog download, or modified compaction source
is used. This fixture therefore certifies the enumerated compaction logic and
requests; it does not certify provider transport or model-catalog parity.

The Rust package and its normal test suite consume committed JSON and require
no Node.js, npm, Pi checkout, Python, or network access. JSON numeric comparison
treats `0` and `0.0` as the same JavaScript number; text, item/property order
inside serialized prompts, fields, arrays, and missing/null values are checked.

Additional offline generators use the same verified checkout and tsx setup:

- `generate-pi-tool-fixtures.mts` → `pi-tools.json`: 187 actual calls through Pi's
  seven tool implementations and TypeBox argument preparation/validation.
- `generate-pi-image-fixtures.mts` → `pi-images.json`: 24 Photon cases, compared
  by exact encoded byte hashes, dimensions, MIME and wording.
- `generate-pi-prompt-fixtures.mts` → `pi-prompts.json`: pinned model-visible
  prompt/tool wording and project instruction loading.
- `generate-pi-llama-fixtures.mts` → `pi-llama.json`: 76 router/HF cases.
  The generator replaces fetch with deterministic responses and records requests;
  private progress helpers are exposed by appending exports to a temporary copy
  of unchanged upstream source. It makes no router/Hugging Face network requests.
  Rust exercises the router fixtures against an actual loopback HTTP server;
  additional tests exercise asynchronous polling, cancellation and rollback.
  Native Hugging Face loopback tests now verify query strings, authorization,
  gated-model responses, quantization, sibling filtering and cancellation.

- `generate-pi-completions-fixtures.mts` → `pi-completions.json`: 143 actual
  compatibility detection, request construction, thinking-format, history,
  image omission, cache and context-limit cases. Private helpers are exposed
  by appended exports in a temporary copy; network access throws.
- `generate-pi-chat-stream-fixtures.mts` → `pi-chat-stream.json`: 53 complete
  streaming responses and 28 partial-JSON/repair cases. Pi's real OpenAI SDK
  receives deterministic SSE through its fetch injection. Rust consumes those
  SSE bytes through an actual loopback HTTP server and compares full persisted
  message fields, ordering, signatures, errors and usage against Pi.

- `generate-pi-chat-http-fixtures.mts` → `pi-chat-http.json`: 27 actual SDK
  HTTP errors, provider retry and affinity-header cases. The Rust test compares
  native loopback requests and errors. SDK `x-stainless-*` reporting is explicitly
  excluded by the no-telemetry rule; other compared headers retain Pi behavior.
- `generate-pi-loop-fixtures.mts` → `pi-loop.json`: actual pinned agent loop
  and compatible provider parser reject every tool call in an output-limited
  response, continue, and execute the recovered call. The Rust worker test
  compares persisted messages and independently checks filesystem effects.

Codex request/catalog fixtures (`pi-codex-requests.json`) execute the pinned
`buildRequestBody`, shared Responses converter, thinking clamp and the explicit
Codex catalog block from `generate-models.ts` followed by its metadata passes.
The generator entrypoint and CLI argument processing are not executed; fetch
throws if reached. 162 cases cover full logical replay, source-model changes,
IDs/signatures, image downgrade, orphan results, cache keys, options and catalog
thinking levels. The native comparison also checks serialized object order.
`reference/openai-codex-models.json` is the resulting offline built-in catalog.

`pi-codex-http.json` uses the actual Codex SSE transport with deterministic fetch
responses and a synthetic JWT. It captures decompressed requests, headers and
final errors, including the dedicated catch-path retry quirks. Native tests use
a loopback HTTP server and a disposable auth store, never real credentials.

`pi-codex-stream.json` executes the complete pinned SSE reader, Codex event map
and Responses state machine. Its 104 cases compare full logical messages,
signatures, ordering, partial argument buffers, errors, usage and service tiers
against native HTTP streams. The native frame-order test separately protects
partial output when a later frame is malformed. Exact V8 error text is also
checked by the dedicated malformed-JSON fixtures described below. Native UTF-16
stream cases retain split text, thinking and tool-argument code units.

`pi-codex-websocket.json` exposes only the pinned private continuation/header/URL
helpers. Rust checks exact serialized continuation bodies and nine URL cases.
`pi-codex-websocket-stream.json` executes Pi's actual public transport against a
small event-compatible fake socket, with deterministic SSE fallback. Its 22
scenarios are replayed through real native loopback WebSocket and HTTP servers.
Only timestamps and runtime-specific diagnostic stack frames are removed from
message comparison. Native cancellation tests additionally verify EOF during a
handshake, a close frame during active output, and a successful fresh request
with the same logical session afterward. All fixture credentials are synthetic.

`pi-idle.json` uses the pinned coding-agent HTTP dispatcher and real loopback
streaming HTTP connections. Both dedicated Codex SSE and the actual compatible
SDK time out while retaining partial output. Abort cases distinguish the
Codex HTTP reader's `This operation was aborted` from the compatible stream's
`Request was aborted`. Rust compares all persisted message fields, normalizing
only timestamps and JSON integer/float representation.

The seven-tool fixtures also capture raw file bytes, non-UTF-8 filenames,
mutation-registration errors and cancellation order, and live bash update
sequences including post-exit cancellation and timeouts. `pi-tools-fd8.json`
contains five actual Pi runs with Debian Bookworm's fd 8.6.0, whose dependency
error wording differs from fd 10.5.0 used by the main oracle.

`generate-pi-surrogate-fixtures.mts` captures exact raw tool JSON and actual
Responses conversion in `pi-surrogate-wire.json`. Three grep/history/provider
cases, 23 tool argument/error/filesystem cases across the seven tools, three
summary cases and six JSON cases cover native UTF-16 preservation. Rust checks
raw code units, 50 KiB truncation metadata, typed history, numbered JSONL reload,
server-side forks, retained attachment references, live wire serialization,
provider sanitization and summary slices. Lone units stay in logical history;
filesystem/argv encoding and provider prose conversion follow Pi's distinct
rules. These fixtures now pass, including the original two failing cases.

`generate-pi-json-error-fixtures.mts` → `pi-json-errors.json` contains 116 actual
V8 JSON.parse diagnostics and pinned Codex SSE outcomes. Tests preserve output
emitted before a malformed later frame and compare native diagnostic text,
including source offsets, line/column positions and split-surrogate snippets.
Only runtime-specific stack traces are excluded.

`generate-pi-oauth-refresh-fixtures.mts` → `pi-oauth-refresh.json` contains nine
actual pinned OpenAI subscription refresh outcomes using synthetic credentials.
It records token-exchange forms, rotated account metadata, expiry boundaries,
invalid credentials and HTTP/JSON errors. Native loopback concurrency tests
also verify shared refresh, cancellation, failed replacement, logout and
browser-login expiry without accessing the user's real credentials.

`generate-pi-usage-checkpoint-fixtures.mts` → `pi-usage-checkpoint.json` contains
three pinned usage histories for numbered-file checkpoints: retained cache-hit
statistics without an assistant in the newest segment, a later cache-rate
replacement, and chronological floating-point cost accumulation. Rust checks
that segment rotation preserves Pi's totals and last cache-hit display.
