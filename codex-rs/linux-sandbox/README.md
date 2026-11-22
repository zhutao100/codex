# Linux sandbox

The standalone `codex-linux-sandbox` helper and `codex sandbox linux` use the same launcher. `landlock` remains a CLI alias, not a second backend.

## Runtime

- A filesystem-restricted policy starts [bubblewrap](src/bwrap.rs) to create a read-only root and bind the permitted writable roots. Protected paths such as `.git` and `.codex` stay read-only inside a writable workspace. The helper isolates the PID namespace and normally mounts a fresh `/proc`; `--no-proc` omits that mount, not the filesystem restrictions.
- The inner process sets `no_new_privs` for restricted policies and installs the [network seccomp filter](src/seccomp.rs) when network access is disabled. Policies with full disk access need no mount launcher.
- The [resolver](src/bwrap_program.rs) prefers the distribution's executable `/usr/bin/bwrap`. Release binaries can also use `codex-resources/bwrap` beside the physical executable if its SHA-256 matches the digest compiled into the binary. Neither `PATH`, the working directory nor runtime environment variables select another launcher.
- Without a usable launcher, a filesystem-restricted command fails before it runs. Install the distribution's `bubblewrap` package for an ordinary Cargo or Bazel build. For a release, extract the complete Linux tarball or install the complete npm package. The standalone `.zst` CLI alone needs the system launcher or its matching resource.

## Packaging and acceptance

The [release workflow](../../.github/workflows/rust-release.yml) builds a static [vendored resource](build-bwrap.sh) on the target architecture and binds its digest to the Rust binary. The release tarball and npm component carry the same bytes. Ordinary builds do not require a C build of bubblewrap.

The Linux [DotSlash manifest](../../.github/dotslash-config.json) selects the complete tarball and runs its platform-named member. npm staging downloads the CLI and resource from the same repository and Actions run, including fork releases.

The [Nix package](../default.nix) uses the same resource contract for its immutable store executable; it does not require `/usr/bin/bwrap` on NixOS.

Qualify both paths in a disposable Linux guest: run the release without the system package, then an ordinary build with the distro package. Check allowed workspace writes, denied writes outside it and under `.git`, network policy, `NoNewPrivs`, retained-terminal input, and the MCP shell subprocess. A missing or mismatched release resource without a system launcher must leave the command's output file absent. A skipped or nested-sandbox test is not evidence of enforcement.
