# Model Client Version

## Contract

- Model discovery uses `CODEX_VERSION_OVERRIDE` when set at compile time, otherwise the compiled `CARGO_PKG_VERSION`.
- Remove prerelease and build metadata: `1.2.3-alpha.4+build.5` and `1.2.3+build.5` both become `1.2.3`.
- Use the same whole version for the `/models` query, `models_cache.json` writes, and cache compatibility checks. TTL and ETag behavior remain unchanged.
- A source build without an override retains the workspace package version, currently `0.0.0`. Runtime environment changes do not change the compiled version; rebuild after changing the override.
- A cache written with the old, mismatched version is ignored and refreshed through normal model discovery. No cache migration or manual deletion is required.

## Verification

- Unit coverage: `codex-models-manager`'s `version_tests` checks compile-time selection and release, prerelease, build-only, and combined suffixes.
- Transport/cache coverage: `models_endpoint::tests::explicit_catalog_reuses_auth_headers_and_query_parameters` checks the actual localhost query, persisted version, and cache reuse for both ChatGPT and API-key authentication.
- Run with the override unset and with release/prerelease/build-metadata overrides. Keep localhost coverage under the repository wrapper rather than bypassing its network guards.

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 CODEX_VERSION_OVERRIDE=1.2.3-alpha.4+build.5 cargo-local test -p codex-models-manager --lib version_tests --quiet -j 2
CODEX_SANDBOX_NETWORK_DISABLED=1 CODEX_VERSION_OVERRIDE=1.2.3-alpha.4+build.5 cargo-local test -p codex-model-provider --lib models_endpoint::tests::explicit_catalog_reuses_auth_headers_and_query_parameters --quiet -j 2
```
