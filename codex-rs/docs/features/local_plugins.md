# Local plugins

## Scope

Local marketplaces provide packaged skills and stdio/HTTP MCP servers. The CLI installs bundles; the existing core skills/MCP runtimes load enabled installations in exec and the direct-core TUI. Installation copies files and does not build or execute package code.

|Supported|Outside the implemented scope|
|---|---|
|Local `.agents/plugins/marketplace.json` catalogs and local sources|Remote/Git/npm downloads or marketplace authentication|
|`.codex-plugin/plugin.json`, skills/supporting assets and MCP configuration|Portable root `plugin.json`, bundled apps, hooks or legacy commands|
|CLI lifecycle commands, qualified skill mentions and `/plugins` diagnostics|Plugin-level mention routing, plugin RPCs or an interactive management dialog|

Unsupported declared capabilities and source/transport types fail explicitly. Presentation metadata does not grant execution privileges. MCP authentication and requirements remain owned by the existing MCP runtime.

## Lifecycle

Build/package executable assets before installing. For a local catalog named `local-tools` containing a plugin named `example`:

```bash
codex plugin marketplace add /absolute/path/to/catalog
codex plugin add example@local-tools
codex plugin list --json
codex plugin disable example@local-tools
codex plugin enable example@local-tools
codex plugin remove example@local-tools
codex plugin marketplace remove local-tools
```

- Restart Codex after bundle or activation changes. Existing sessions and MCP connections do not hot-reload installations.
- Repeating `add` replaces the installed copy, including rebuilt binaries with an unchanged manifest version. Source directories are never deleted.
- Marketplace removal requires removing its installed plugins first. Lifecycle commands also accept `PLUGIN --marketplace MARKETPLACE` and `--json`.
- Select a packaged skill with `$example:skill-name` or the skills picker. `/plugins` shows configured installations.
- A session override such as `codex -c 'plugins.example@local-tools.enabled=false'` changes effective loading without modifying the stored installation.

## State and configuration decisions

[Plugin storage](../../core/src/plugins/store.rs) keeps a single local installation under `$CODEX_HOME/plugins/cache/MARKETPLACE/PLUGIN/local`. Manifest versions are display metadata. Registration stores a canonical local catalog directory; copying excludes `.git` and root build caches `.build`/`target` while preserving executable permissions and valid internal symlinks. Escaping/absolute symlinks and special files are rejected.

CLI mutations use the atomic TOML writer, preserve unrelated settings/comments and serialize package operations with a lock. A staged bundle is validated before publication; publication/config failures restore the previous copy. Runtime loading uses effective trusted configuration. An invalid installation produces a diagnostic without hiding unrelated skills.

[Skill loading](../../core/src/skills/manager.rs) caches only non-plugin skills by cwd. Each lookup applies the caller's plugin configuration, including forced listings and cache refreshes. Never cache one session's plugin activation in the shared cwd cache.

- Skills default to `./skills`, or use manifest-selected directories. Their existing parser and per-path disabling rules still apply; they retain user scope.
- Qualified names are `PLUGIN:SKILL`, including dotted namespaces. Equal plugin names from different marketplaces can be ambiguous; explicit links to the installed `SKILL.md` disambiguate. A structured skill input alone does not change the core's existing injection trigger rules.
- The manifest accepts inline `mcpServers`, a declared JSON file or default `.mcp.json`; both a server map and an `mcpServers` wrapper work. `${CODEX_PLUGIN_ROOT}` and `${CLAUDE_PLUGIN_ROOT}` expand within JSON strings; arbitrary environment interpolation is not added.
- Stdio defaults cwd to the installed bundle. HTTP `headers` maps to `http_headers`. Explicit `[mcp_servers]` settings win over plugin declarations; colliding plugin server names use sorted plugin-ID order and produce a warning.
- Plugin MCP entries pass through the existing requirements filter. Enabling a package does not bypass MCP restrictions, approvals or shell sandboxing.

## Validation entry points

|Concern|Source/tests|
|---|---|
|Manifest, lifecycle, copy/rollback and MCP merge|[Plugin implementation and unit tests](../../core/src/plugins/mod.rs)|
|Catalog, explicit injection and session overrides|[Core integration tests](../../core/tests/suite/plugins.rs)|
|CLI installation/toggle/removal|[CLI integration tests](../../cli/tests/plugins.rs)|
|Qualified selections and diagnostics|[TUI plugin handling](../../tui/src/chatwidget/plugins.rs)|

For release integration with Curl Safari, use [verify-local-plugins.py](../../scripts/verify-local-plugins.py) only in a disposable macOS VM with the built package, helper access and Safari automation configured. Its `--codex`, `--plugin-source`, `--safari-cli` and `--artifacts` arguments identify guest paths. It checks authenticated localhost fetches, skill/MCP lifecycle and TUI overrides through deterministic Responses fixtures; it does not establish fresh permission onboarding or a hosted model's autonomous tool-selection quality.
