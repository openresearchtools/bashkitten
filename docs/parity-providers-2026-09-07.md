# Provider completion audit, 7 September 2026

Reference: Pi commit `9841914c71a74d81abe07f751aefd271fd924e63`, verified in
`/run/media/user/Data/bashkitten-builds/pi-pinned` before implementation.
Only OpenAI subscription, configured OpenAI-compatible APIs, and llama.cpp
inference through that compatible path are in scope. No provider, proxy,
credential-import path, or background destination was added.

## Malformed streams

`src/json_error.rs` reproduces the JSON.parse diagnostics observable in the
pinned Pi execution runtime (Node 22 / V8). It walks the JSON grammar natively
without recursion, reporting UTF-16 offsets, CR/LF/CRLF positions, number and
escape failures, property/array delimiters, and V8's token-context excerpts.
Lone-surrogate unexpected tokens and snippets are held in a native UTF-16
provider error through redaction, response assembly and persisted history.
It introduces no JS dependency into Rust builds, tests or runtime.

`generate-pi-json-error-fixtures.mts` captures 116 JSON.parse diagnostics and
actual Pi Codex SSE outcomes. Native tests compare errors and the preceding
assistant output, including malformed input after output has arrived. All
fixtures are local and synthetic. Runtime version is recorded in the fixture
because JSON.parse wording is supplied by V8, not the Pi TypeScript source.
Source: `packages/ai/src/api/openai-codex-responses.ts`, `parseSSE` and
`streamWebSocketEvents` (the exact exception prefixes are preserved).

The full Codex WebSocket fixture now includes malformed first/later frames and
JSON null, including the lack of SSE fallback after a protocol error and the
successful fresh socket on the next turn. The compatible-provider fixture adds
malformed first/later frames and SDK error-message propagation. Two additional
HTTP cases preserve raw Unicode metadata and a surrogate split at the SDK
4000-code-unit error-body cap. This corrects
the previous Rust-specific serde error prefix and the loss of the provider's
error message.

## OAuth boundaries

Sources: `packages/ai/src/auth/resolve.ts` (`resolveStoredOAuth`) and
`packages/ai/src/auth/oauth/openai-codex.ts` (`readTokenResponse`,
`credentialsFromToken`, `refreshAccessToken`). The existing five-minute
minimum-validity window, 15-second refresh timeout, double-checked lock and
rotation-before-unlock behavior are retained.

Login and refresh now share one token decoder. A refreshed access token is
validated before publication, and the returned accountId replaces the prior
account metadata. Previously an invalid refreshed token could overwrite valid
credentials before JWT validation; refresh also retained stale accountId.
Numeric zero, negative and fractional expiry durations match Pi rather than
being rejected by the earlier Rust refresh-only decoder. JWTs require exactly
three parts. Nine actual pinned refresh outcomes record request forms,
credentials, account changes, invalid JWTs, missing fields and HTTP/JSON errors.

Native loopback tests launch eight simultaneous expired-token consumers and
verify exactly one refresh. They also prove logout waits for a real in-flight
refresh then removes its result, cancellation releases the credential lock,
and failed validation preserves the original credential file byte-for-byte.
Every auth test uses disposable directories and synthetic JWTs; real user
credentials are never read or changed.

The documented BashKitten 15-minute browser-login bound now covers token
exchange as well as waiting for authorization. Token-bearing error fields are
redacted even in malformed/truncated or embedded JSON. Redaction is the explicit
AGENTS.md secret-handling divergence from Pi's verbatim token-response errors;
fixture expectations redact only those synthetic secret fields.

## Cached sockets

Source: `packages/ai/src/api/openai-codex-responses.ts`, `acquireWebSocket`,
`scheduleSessionWebSocketExpiry`, and the 300-second / 55-minute constants.
Native loopback tests with virtual Tokio time verify five-minute idle expiry,
55-minute age replacement at acquisition, survival of an active request beyond
that age, ephemeral concurrent leases, reuse after completion, and isolation
between account IDs. Socket release now publishes its reusable flag and idle
task under one lock, so a concurrent acquire can always abort the idle reader
before using the cached socket.

## Unicode provider boundaries

Pi's `sanitizeSurrogates` is applied at the same selected prose boundaries:
compatible system text, user/assistant text, tool-result text, and thinking
converted to text. Codex system instructions, tool argument JSON and opaque
reasoning signatures retain their native UTF-16 values through lossless JSON.
Native
Codex/compatible streams preserve individual UTF-16 text and reasoning deltas;
separately arriving high/low units form the same final character as Pi.
Four actual Codex SSE and four compatible SSE cases exercise split text,
thinking and tool arguments, including a retained lone unit in partial JSON.
Three extra partial-parser cases check literal surrogate values/keys and
escaped private-use Unicode without representation collisions.
Request conversion continues to use the seven existing JSON-schema tools.

## Validation

The new fixtures supplement the existing request/catalog, transport/retry,
stream/signature, cached continuation, header-timeout and cancellation suites.
The native suite consumes committed JSON only. Regeneration requires the
separate pinned checkout and its development-only TypeScript runtime.
The provider fixtures contain 143 compatible requests, 162 Codex requests,
53 compatible streams, 28 partial-JSON cases, 104 Codex streams, 22 WebSocket
streams, 116 malformed-JSON cases, 27 compatible HTTP cases, and nine OAuth
refresh cases. Their focused native suites pass, including the virtual-time
socket expiry and browser-login expiry tests. The final combined test suite
and Clippy with warnings denied also passed. These checks establish the tested
boundaries, not exhaustive coverage of every response or real account state.

## Live credential comparison

The existing credential was rejected by the provider during release checks.
A minimal request through the previously installed September 5 CLI/agent and
through the new debug CLI/agent produced the same exact error: `Provided
authentication token is expired.` Both temporary sessions were stopped. No Web
instance, login, logout, forced refresh or credential import was involved.
Credential file bytes were unchanged; stored and JWT expiry agreed and were in
the future. This establishes that the rejection is also present in the old
binary; it does not establish the server's reason. Safe evidence is recorded in
`tests/live/2026-09-07-codex-auth.json`.

After the user reported provider login, the installed release passed a real Luna
seven-tool task and its supplied/independent tests, with provider-returned cache
usage. See `tests/live/2026-09-07-luna-installed.json`. The earlier rejection is
retained as diagnosis history, not a current release blocker.
