#!/usr/bin/env bash
# Build a standalone resource on a Linux runner of the target architecture.
# Static linkage lets GNU and musl Rust binaries use the same verified program.
set -euo pipefail
out=${1:?output binary path required}
vendor=$(cd -- "$(dirname -- "$0")/../vendor/bubblewrap" && pwd)
build=$(mktemp -d)
trap 'rm -rf -- "$build"' EXIT
printf '%s\n' '#define PACKAGE_STRING "bubblewrap (Codex)"' > "$build/config.h"
mkdir -p -- "$(dirname -- "$out")"
read -ra cflags <<< "$(pkg-config --cflags libcap)"
read -ra libs <<< "$(pkg-config --static --libs libcap)"
cc -O2 -static -D_GNU_SOURCE -I"$build" -I"$vendor" "${cflags[@]}" \
    "$vendor"/{bubblewrap,bind-mount,network,utils}.c "${libs[@]}" -o "$out"
