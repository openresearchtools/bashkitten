#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build_root=${BASHKITTEN_BUILD_ROOT:-/run/media/user/Data/bashkitten-builds}
image=${BASHKITTEN_BUILD_IMAGE:-localhost/bashkitten-build:bookworm}

mkdir -p "$build_root/artifacts" "$build_root/cargo-home" "$build_root/target"

podman build -f "$repo_dir/Containerfile.build" -t "$image" "$repo_dir"
podman run --rm \
    --userns=keep-id \
    -e CARGO_HOME=/build/cargo-home \
    -e CARGO_TARGET_DIR=/build/target \
    -v "$repo_dir:/source:ro" \
    -v "$build_root:/build:rw" \
    "$image"
