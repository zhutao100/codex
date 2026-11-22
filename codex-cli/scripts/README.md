# npm releases

Use the staging helper to generate npm tarballs from a matching Rust release:

```bash
./scripts/stage_npm_packages.py \
  --release-version "$version" \
  --package codex \
  --package codex-responses-api-proxy \
  --package codex-sdk
```

This downloads the native artifacts once, populates `vendor/` for each package, and writes tarballs to `dist/npm/`.

For manual staging, run `codex-cli/scripts/install_native_deps.py --workflow-url "$workflow_url" "$dest"` and pass `--vendor-src "$dest/vendor"` to `build_npm_package.py`. The Linux CLI and SDK need the `codex-resources/bwrap` resource from the same build as `codex`; an older or incomplete artifact set is not interchangeable.
