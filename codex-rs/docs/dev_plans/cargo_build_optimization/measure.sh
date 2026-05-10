#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "${script_dir}/../../.." && pwd -P)"
cargo_cmd="${CARGO_CMD:-${repo_root}/scripts/cargo-local}"
out_dir="${CODEX_BUILD_OPT_OUT_DIR:-/tmp/codex-build-optimization}"
skip_builds=0

usage() {
    cat <<'EOF'
Usage: measure.sh [--skip-builds]

Captures codex-cli dependency-tree output and release/release-fast timings.
Outputs are written outside the repository because Cargo tree output contains
absolute local paths.

Environment:
  CARGO_CMD                 Cargo wrapper to use (default: scripts/cargo-local).
  CODEX_BUILD_OPT_OUT_DIR   Output directory (default: /tmp/codex-build-optimization).
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --skip-builds)
            skip_builds=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            exit 2
            ;;
    esac
done

mkdir -p "${out_dir}"
cd -- "${repo_root}"

"${cargo_cmd}" tree -p codex-cli -e normal >"${out_dir}/codex-cli-tree-normal.txt"
"${cargo_cmd}" tree -p codex-cli -e features >"${out_dir}/codex-cli-tree-features.txt"
"${cargo_cmd}" tree -p codex-cli --duplicates >"${out_dir}/codex-cli-tree-duplicates.txt" || true

if [ "${skip_builds}" -eq 1 ]; then
    printf 'Wrote dependency captures to %s\n' "${out_dir}"
    exit 0
fi

"${cargo_cmd}" clean -p codex-cli --release \
    >"${out_dir}/codex-release.clean.stdout" \
    2>"${out_dir}/codex-release.clean.stderr"
RUSTC_WRAPPER= CODEX_SANDBOX_NETWORK_DISABLED=1 \
    /usr/bin/time -p \
    "${cargo_cmd}" build -p codex-cli --bin codex --release --timings \
    >"${out_dir}/codex-release.stdout" \
    2>"${out_dir}/codex-release.stderr"

"${cargo_cmd}" clean -p codex-cli --profile release-fast \
    >"${out_dir}/codex-release-fast.clean.stdout" \
    2>"${out_dir}/codex-release-fast.clean.stderr"
RUSTC_WRAPPER= CODEX_SANDBOX_NETWORK_DISABLED=1 \
    /usr/bin/time -p \
    "${cargo_cmd}" build -p codex-cli --bin codex --profile release-fast --timings \
    >"${out_dir}/codex-release-fast.stdout" \
    2>"${out_dir}/codex-release-fast.stderr"

printf 'Wrote build optimization captures to %s\n' "${out_dir}"
