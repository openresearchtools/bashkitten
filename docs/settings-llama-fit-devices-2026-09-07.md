# llama.cpp device and fit controls

BashKitten remains pinned to Pi commit `9841914c71a74d81abe07f751aefd271fd924e63`.
The installed flag and preset behavior is documented by the upstream
[llama.cpp server README](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md).
Pi's llama.cpp extension consumes router model metadata, including the context
reported by `meta.n_ctx` or `meta.n_ctx_train`. It does not provide a settings
surface for the host process's accelerator selection or memory fitting.

The user-requested Debian integration adds a narrow local difference: the
settings page invokes the installed `/usr/bin/llama-server --list-devices`,
passing the configured `CUDA_VISIBLE_DEVICES` and `GGML_VK_VISIBLE_DEVICES`
environment values. The parser retains the exact llama.cpp device ID and the
human-readable name and memory values. Selected IDs are persisted and passed as
the native `--device` value; the empty selection leaves llama.cpp's default
device choice. Output and line counts are bounded so a diagnostic cannot grow
without limit.

The installed binary's `--help` output is the capability source for fitting and
generation controls. When the flags are present, BashKitten emits `--fit`,
`--fit-target`, `--fit-ctx`, and `--n-predict` in the router launch arguments
and their native INI equivalents. Model presets can override fit state, fit
margin, minimum fit context, and maximum new tokens. A router context of zero
omits `--ctx-size`, matching llama.cpp's documented "loaded from model"
default and allowing `--fit` to adjust the unset context; a nonzero value is an
explicit context cap. This uses the installed llama.cpp behavior instead of
introducing a second memory estimator. The router's reported model context
remains the source for Pi-equivalent provider context and compaction
calculations.

The same native rule applies to GPU layers: llama.cpp rejects fitting when
`n_gpu_layers` was explicitly set. Therefore Auto/all emits `-ngl 999` when
fit is off, but leaves the layer argument unset while fit is active. Explicit
CPU or layer-count modes keep their requested `-ngl` value and emit `fit = off`.

The device refresh endpoint also accepts the settings form's unsaved visibility
values. A successful refresh removes selected IDs that are absent from the
current listing, while a failed listing preserves the draft so a transient
probe error cannot erase it.
