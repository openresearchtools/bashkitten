# Fresh native llama.cpp acceptance

Pi reference: `9841914c71a74d81abe07f751aefd271fd924e63`.

Result: **passed**, using the current native debug CLI and agent binaries from
`/run/media/user/Data/bashkitten-builds/target/debug` and the already cached
Qwen3 0.6B Q4_K_M GGUF. No download or Web process was needed for this run.

The isolated native CLI launcher executed the installed `llama-server` in router
mode through one temporary systemd user service. It generated native llama INI
presets, kept autoload disabled, and did not pass a single-model router argument.
An explicit loopback load made the configured model available in the unified
native registry with an 8192-token context and `off` thinking.

A native CLI session omitted its thinking override and correctly selected the
preset's `off` default. Its separate native worker called `write`, produced
`acceptance.txt` with exactly `NATIVE-LLAMA-20260907` and a newline, and returned a
final assistant answer. The model stayed loaded after the turn and after the
worker was stopped. It unloaded only after the explicit router unload request.
The retained GGUF's SHA-256 remained unchanged.

Both temporary services were stopped. The existing production Web and router
service states and PIDs were identical before and after the run. The test used
separate configuration, runtime, session and work directories under
`/run/media/user/Data/bashkitten-builds/llama-live-2026-09-07`; no installed
configuration or service definition was changed.

[The recorded JSON](2026-09-07-llama-router-inference.json) contains the native
arguments, generated INI, actual router catalogs, registry entry, complete session
entries, independent expected-byte check, process paths, service log excerpts and
cleanup state. This run verifies CLI/router/worker integration. Authenticated Web
settings and cancellation failure paths are covered by the separate deterministic
tests; this run does not claim a fresh browser/GTK acceptance test.
