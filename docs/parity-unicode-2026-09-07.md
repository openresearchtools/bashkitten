# Native UTF-16 parity verification, 7 September 2026

Reference HEAD was verified before implementation:
`9841914c71a74d81abe07f751aefd271fd924e63` in
`/run/media/user/Data/bashkitten-builds/pi-pinned`.

The original two raw `grep` fixtures are repaired. BashKitten now retains
JavaScript UTF-16 code units in native Rust `JsString` values. Its JSON boundary
codec emits normal JSON string escapes, including `\ud800`, and decodes them
without replacing or deleting data. Temporary tagged values stay inside Rust;
ordinary input objects using the same tag names are escaped and restored,
including nested objects, repeated keys and unpaired-surrogate property names.
No JavaScript runtime or dependency is introduced in the build or application.

The model-visible behavior follows the pinned source rather than introducing
a Unicode normalization policy:

- `coding-agent/src/core/tools/grep.ts` and `truncate.ts`: slice lines at 500
  UTF-16 units, retain a split pair in results and truncation metadata, and use
  Node-compatible UTF-8 byte counts when applying the 50 KiB limit.
- `coding-agent/src/core/tools/{read,write,edit,grep,find,ls,bash}.ts`: native
  argument validation and diagnostic strings preserve lone units. Filesystem
  writes and child-process arguments use Node's U+FFFD replacement conversion.
  `edit` can match one half of a valid surrogate pair, retains lone units in
  its diff/patch, and keeps supplementary-letter NFKC fuzzy matching intact.
- `coding-agent/src/core/compaction/{utils,compaction}.ts`: context estimates
  count original code units; summary truncation can split a pair; summaries
  retain those units until provider conversion.
- `ai/src/utils/sanitize-unicode.ts` and `api/openai-responses-shared.ts`:
  provider-bound prose removes lone units at Pi's specified conversion points;
  full logical history, tool arguments, JSONLs, forks and live output retain
  their original units. Provider stream/HTTP changes are recorded separately.

`scripts/generate-pi-surrogate-fixtures.mts` was run against the verified,
unchanged source using the existing offline fixture tsconfig. The committed
`tests/fixtures/pi-surrogate-wire.json` contains 3 tool/history/provider cases,
23 tool argument/error/filesystem cases, 3 summary cases and 6 JSON cases.
The find argument cases use a Git directory, matching the ordinary fd fixture
setup and avoiding differences in optional flags accepted by fd 8 versus 10.

`tests/pi_surrogate.rs` compares exact raw tool JSON, code units, byte truncation
metadata, typed history, token estimates, numbered JSONL persistence and reload,
server-side forks, live JSON serialization, provider conversion, compaction
transcripts, diagnostic wording and filesystem effects against those fixtures.
The existing 187-call tool and compaction differential suites also pass.

Verification command (native Rust, offline, in the existing build image):

```text
cargo test --offline --locked --test pi_tools --test pi_compaction --test pi_surrogate
```

All five Rust test functions passed. Pi fixture generation remains a separate
development operation; ordinary Rust tests consume only committed JSON.
