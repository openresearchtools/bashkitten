# Pinned Pi behavioral reference

BashKitten parity work is pinned to:

- Repository: <https://github.com/earendil-works/pi>
- Version: post-`v0.85.0` (GPT-6 Astra catalog update)
- Commit: `9841914c71a74d81abe07f751aefd271fd924e63`

Updated on 2026-09-05. Catalog/pricing/thinking metadata comes from `packages/ai/scripts/generate-models.ts`, including commit `17de82d7bea18a6589677a9761baabc2060c9efb`. OAuth comes from `packages/ai/src/auth/oauth/openai-codex.ts`, `pkce.ts`, and `device-code.ts` (unchanged since the previous pin). The Web callback presentation, authenticated control endpoints and 15-minute browser-login lifetime are the documented BashKitten differences. Model definitions and costs are shipped locally; no runtime external catalog lookup is added.

The reference checkout used during development lives outside this repository under the local data-drive build directory. Pi source code is a behavioral specification only and is not a BashKitten runtime dependency.
