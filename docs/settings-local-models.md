# Settings: local model sources and GPU visibility

The explicit user request on 2026-09-05 adds settings tabs (App, Subscriptions,
APIs, llama.cpp), local Hugging Face cache and conventional LM Studio folder
listings, user-selected additional model folders, and llama.cpp GPU visibility
environment settings. These are narrow additions to the Debian launcher and Web
settings, documented before implementation; they do not change pinned Pi's
provider requests, router operations, model selection, or its seven tools.

Pi remains pinned to `9841914c71a74d81abe07f751aefd271fd924e63`. Its
`packages/coding-agent/src/extensions/llama/client.ts` and `provider.ts` discover
models through the configured router. BashKitten supplements the setup page with
local GGUF discovery because the user explicitly requested folders outside the
router's single models directory. Discovered files do not enter the launchable
registry until the user saves a preset. No scan loads, copies, deletes, or downloads
a model, and no remote request is made by the local scan.

Sources are the configured router directory, the llama.cpp cache, the Hugging
Face hub cache (`HF_HUB_CACHE`, legacy `HUGGINGFACE_HUB_CACHE`, `HF_HOME/hub`, or
`XDG_CACHE_HOME`/`~/.cache/huggingface/hub`), `~/.lmstudio/models`, the conventional
legacy `~/.cache/lm-studio/models`, and configured absolute custom directories.
Hugging Face snapshot symlinks retain their logical GGUF path so multi-part files
remain together. Canonical file identities deduplicate overlapping sources and
cache snapshots. Symbolic directory cycles are bounded by visited directory
identities. Split GGUF files form one entry keyed by their first shard; incomplete
sets are listed as incomplete and cannot be saved. `mmproj` files are companions,
not standalone chat models. Missing and unreadable source folders remain visible
with their state. The scanner reads directory entries and file metadata only;
it does not infer context, reasoning, or other model capabilities from filenames.

A selected local file is stored as `ModelPreset.llama_model_path` and written as
native llama.cpp `model = /absolute/file.gguf` in the preset's INI section. Advanced
INI entries remain native, but conflicting model-source keys must be removed
when a local file is selected. This prevents changing the selected file through
an unrelated override. Paths with line breaks, comment delimiters, or trailing
whitespace that the installed INI grammar cannot round-trip are rejected clearly.

GPU visibility is an explicit `gpu_environment` map. It permits only
`CUDA_VISIBLE_DEVICES` and `GGML_VK_VISIBLE_DEVICES`, the selectors for the two
Debian backend modes. Values are passed directly as environment entries on the
`llama-server` command (no shell interpolation), and inherited by its model
children. Omission retains the service's existing environment; an empty value
hides all GPUs. Configuration applies when the user restarts the router. It does
not change the Web UI or session process environment and never changes systemd's
global user-manager environment. CUDA accepts its native index/UUID/MIG syntax;
Vulkan accepts nonnegative indices separated by commas or spaces. Backend-native
index validation occurs in llama.cpp, since probing hardware is not necessary
to save a selector.

Reference sources:

- [Hugging Face cache environment variables](https://huggingface.co/docs/huggingface_hub/package_reference/environment_variables)
- [Hugging Face cache layout](https://huggingface.co/docs/huggingface_hub/en/guides/manage-cache)
- [LM Studio local model import and link support](https://lmstudio.ai/docs/cli/local-models/import)
- [Installed llama.cpp router model sources and INI](https://github.com/ggml-org/llama.cpp/blob/a30273376ef669023334fc20ad02ae4ed8196a65/tools/server/README.md#model-sources)
- [Installed Vulkan GPU selector](https://github.com/ggml-org/llama.cpp/blob/a30273376ef669023334fc20ad02ae4ed8196a65/ggml/src/ggml-vulkan/ggml-vulkan.cpp#L6784)
- [CUDA environment selectors](https://docs.nvidia.com/cuda/cuda-programming-guide/05-appendices/environment-variables.html)

Conventional LM Studio directories are fallback discovery candidates, not a claim
that an LM Studio installation exists or that its settings have been imported.
The ordinary folder picker supplies other locations explicitly.

The chat header, default working folder, models directory, and additional model
folders all use the same folder picker. It offers subfolders, parent navigation,
an absolute path, and creating a subfolder. When opening at a configured path
that does not exist yet, it starts at the nearest readable parent. The user still
chooses the folder explicitly; opening the picker does not alter any setting.
