set working-directory := "codex-rs"
set positional-arguments

cargo_cmd := "cargo-local"

# Display help
help:
    just -l

# `codex`
alias c := codex
codex *args:
    {{cargo_cmd}} run --bin codex -- "$@"

# `codex exec`
exec *args:
    {{cargo_cmd}} run --bin codex -- exec "$@"

# Run the CLI version of the file-search crate.
file-search *args:
    {{cargo_cmd}} run --bin codex-file-search -- "$@"

# Build the CLI and run the app-server test client
app-server-test-client *args:
    {{cargo_cmd}} build -p codex-cli
    {{cargo_cmd}} run -p codex-app-server-test-client -- --codex-bin "$$({{cargo_cmd}} --print-target-dir)/debug/codex" "$@"

# format code
fmt:
    {{cargo_cmd}} fmt -- --config imports_granularity=Item 2>/dev/null

fix *args:
    {{cargo_cmd}} clippy --fix --all-features --tests --allow-dirty "$@"

clippy:
    {{cargo_cmd}} clippy --all-features --tests "$@"

install:
    rustup show active-toolchain
    {{cargo_cmd}} fetch

# Run `cargo nextest` since it's faster than `cargo test`, though including
# --no-fail-fast is important to ensure all tests are run.
#
# Run `cargo install cargo-nextest` if you don't have it installed.
test:
    {{cargo_cmd}} nextest run --no-fail-fast

# Build and run Codex from source using Bazel.
# Note we have to use the combination of `[no-cd]` and `--run_under="cd $PWD &&"`
# to ensure that Bazel runs the command in the current working directory.
[no-cd]
bazel-codex *args:
    bazel run //codex-rs/cli:codex --run_under="cd $PWD &&" -- "$@"

bazel-test:
    bazel test //... --keep_going

bazel-remote-test:
    bazel test //... --config=remote --platforms=//:rbe --keep_going

build-for-release:
    bazel build //codex-rs/cli:release_binaries --config=remote

# Run the MCP server
mcp-server-run *args:
    {{cargo_cmd}} run -p codex-mcp-server -- "$@"

# Regenerate the json schema for config.toml from the current config types.
write-config-schema:
    {{cargo_cmd}} run -p codex-core --bin codex-write-config-schema

# Regenerate vendored app-server protocol schema artifacts.
write-app-server-schema *args:
    {{cargo_cmd}} run -p codex-app-server-protocol --bin write_schema_fixtures -- "$@"

# Tail logs from the state SQLite database
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; {{cargo_cmd}} run -p codex-state --bin logs_client -- "$@"
