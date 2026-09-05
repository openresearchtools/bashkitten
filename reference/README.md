# Inert Pi documentation reference

`pi-documentation.tar.gz` contains the unmodified `README.md`, `docs/` and
`examples/` from `packages/coding-agent` at Pi commit
`9841914c71a74d81abe07f751aefd271fd924e63`. It is documentation data, never loaded,
compiled, installed as a package, or executed by BashKitten. No Node/Pi dependency
is introduced. The archive expands under `/usr/share/doc/bashkitten/pi-reference`
so the exact pinned system-prompt references name actual local files. The Pi MIT
license is retained in `PI-LICENSE` and installed beside those files.

SHA-256: `e14e4be16a1fe5107bf38c68736165031c9935277f149afbac39088209843f74`.

Reproduce with `git archive <pin>:packages/coding-agent README.md docs examples`
and `gzip -n`, using the commit above. The prompt template in
`src/prompts/pi-default-system.txt` is the default template literal copied
verbatim from that commit's `src/core/system-prompt.ts`; only its interpolation
values are supplied by Rust.
