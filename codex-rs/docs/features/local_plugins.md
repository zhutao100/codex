# Local Plugins Backport

## Contract

- Scope: local `.agents/plugins/marketplace.json` directories, local plugin sources, `.codex-plugin/plugin.json`, packaged skills/supporting files, and stdio/HTTP MCP servers.
- No app-server migration or new extension framework. The CLI manages bundles; the existing core skills/MCP runtimes consume enabled installations in both exec and the direct-core TUI.
- Prerequisite: qualified skill mentions (`$plugin:skill`), including dotted plugin namespaces. Ordinary `$skill.suffix` parsing retains its previous behavior.
- Not included: remote/Git/npm catalogs or downloads, marketplace policy synchronization, portable root `plugin.json`, bundled apps/hooks/legacy commands, plugin-level mention routing, plugin RPCs, or an interactive plugin-management dialog.
- Unsupported declared apps/hooks/commands and unsupported source/transport types fail explicitly. Presentation metadata is not an authorization policy.
- Installation honors availability but does not implement marketplace authentication flows or automatic default installation. MCP authentication remains an existing server-configuration concern.

## Workflow

Build any package executables before installation. For Curl Safari, run these commands from its project directory:

```bash
./build.sh
codex plugin marketplace add "$PWD"
codex plugin add curl-safari@curl-safari-local
codex plugin list --json
```

Start a new Codex process, then select `$curl-safari:curl-safari` or use the existing skills picker. `/plugins` displays the session's configured installations; `/skills` retains its existing behavior. The skill locates its executable and helper relative to its installed `SKILL.md`.

```bash
codex plugin disable curl-safari@curl-safari-local
codex plugin enable curl-safari@curl-safari-local
codex plugin remove curl-safari@curl-safari-local
codex plugin marketplace remove curl-safari-local
```

- `add`, `remove`, `enable`, and `disable` also accept `PLUGIN --marketplace MARKETPLACE` and `--json`.
- Marketplace registration stores a canonical local directory. Installation reads its current manifest and copies the complete bundle, excluding `.git` and root build caches `.build`/`target`; it does not build or execute package code.
- Repeating `add` replaces the installed copy, including rebuilt binaries with unchanged manifest versions. Build/package before adding, and restart Codex after all mutations. Running sessions and their established MCP connections are not hot-reloaded.
- Marketplace removal requires removing its installed plugins first. Package source directories are never removed.
- Curl Safari still requires its local helper and macOS cookie-access permissions. Installation does not grant Full Disk Access or start the helper.

## State And Loading

```toml
[marketplaces.curl-safari-local]
source_type = "local"
source = "/absolute/path/to/codex-plugin-curl-safari"

[plugins."curl-safari@curl-safari-local"]
enabled = true
```

- State lives under `CODEX_HOME`, including `$CODEX_HOME/plugins/cache/MARKETPLACE/PLUGIN/local`. This backport keeps one active local copy, not upstream's version-selection/migration logic. Manifest versions remain display metadata.
- CLI mutations edit the user config with the existing atomic TOML writer and preserve unrelated settings/comments. A package-operation lock serializes concurrent plugin commands. A staged copy is validated before publication; publication/config failures restore the previous copy.
- Runtime loading uses the effective trusted config stack. Missing or invalid bundles do not hide unrelated skills; skill-load errors and `/plugins` diagnostics identify invalid installations.
- Skills default to `./skills`, or use a manifest `skills` string/list of directories. The existing skill parser supplies metadata, instructions, dependencies, and per-path skill disabling. Plugin skills retain user scope; no protocol-enum change is needed.
- Plugin skill names are `PLUGIN:SKILL`. Equal plugin names from different marketplaces remain ambiguous for plain-name selection; explicit `[$PLUGIN:SKILL](/installed/path/SKILL.md)` links disambiguate them. The old core's rule that structured skill inputs alone do not trigger injection is unchanged.
- MCP configuration comes from inline `mcpServers`, a declared `./file.json`, or default `.mcp.json`. Both a server map and an `mcpServers` wrapper are accepted. `${CODEX_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_ROOT}` expand inside JSON string values; no arbitrary environment-variable expansion is added.
- Stdio defaults its working directory to the installed package root. HTTP `headers` maps to the existing `http_headers` field. Servers retain their declared names. Explicit `[mcp_servers]` entries win; duplicate plugin server names use the first plugin in sorted ID order and produce a warning.
- Plugin MCP servers enter the existing requirements filter before startup. Enabling a plugin does not bypass MCP requirements, command approvals, or shell sandbox settings.
- Package identities and paths are validated at the external input boundary. Escaping/absolute symlinks and special files are rejected; internal relative symlinks and executable permissions are preserved.

## Verification

Focused coverage:

- `core::plugins`: lifecycle, config preservation, copied executables/symlinks, rejected inputs, replacement failure, skill namespacing/toggles, MCP config precedence and root expansion.
- `core/tests/suite/plugins.rs`: installed skill catalog and explicit instruction injection into a captured Responses request.
- `cli/tests/plugins.rs`: marketplace/install/list/toggle/remove command round trip and invalid selections.
- TUI tests: qualified selection-to-path mapping and `/plugins` dispatch.

VM-only release check:

```bash
python3 scripts/verify-local-plugins.py \
  --codex /guest/path/codex \
  --plugin-source /guest/path/codex-plugin-curl-safari \
  --safari-cli /guest/path/macos-safari-browser-use/scripts/safari \
  --artifacts /guest/path/artifacts
```

Prerequisites: disposable Apple Silicon macOS VM, Python 3.13+, Safari automation permissions, built Curl Safari app/client, and cookie access for the helper's execution context. Use the GhostVM safe-testing workflow with read-only host inputs and writable artifact export; stop the VM and restore its config afterward.

The probe establishes a synthetic localhost login in guest Safari, runs the installed unmodified helper/client through a real Codex shell tool call, and checks the authenticated result in the next model request. It also verifies absence after disable/removal, re-enabling, and a separate bundled MCP tool call plus disable. Responses are deterministic fixtures, not a hosted-model decision-quality test. Only structural evidence is exported, not model request bodies or Safari cookie contents.

### Verified Results

Verified on October 7, 2026:

- Production build: `cargo-local build --release --bin codex`, published to `target/release/codex`; SHA-256 `30314e2d8c6ae78347082b8ad51697ab4a214fa244843843d4177f29e233b81e` matched the guest copy.
- macOS 15.6.1 GhostVM, reverted from `automation-ready`: built the unmodified Curl Safari Go client and Swift helper, then verified all six release-binary probe cases, including a fresh Safari cookie authenticated fetch and a real MCP call. Eight Responses requests were checked.
- Release TUI PTY check: `/plugins` listed the installation; selecting the qualified skill and submitting a turn produced its catalog entry and full instructions in the captured request, then rendered the response.
- Rust checks: 1,191 core unit tests and one focused core HTTP integration test passed; 927 TUI unit tests and nine TUI integration tests passed; all 58 CLI tests passed in the VM, with subprocess tests using the release binary. Seven existing tests remained ignored across these scopes.
- `just-local fmt`, scoped Clippy fixes for core/TUI/CLI, and regenerated config-schema validation passed. The full workspace `--all-features` test suite was not run.
- The helper ran under the guest's pre-authorized automation context. This verifies real Safari cookie access and the client/helper/curl chain, not fresh Full Disk Access onboarding or a hosted model's autonomous tool selection.
