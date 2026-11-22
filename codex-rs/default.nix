{
  bubblewrap,
  openssl,
  rustPlatform,
  stdenv,
  pkg-config,
  lib,
  ...
}:
rustPlatform.buildRustPackage (_: {
  env = {
    PKG_CONFIG_PATH = "${openssl.dev}/lib/pkgconfig:$PKG_CONFIG_PATH";
  };
  pname = "codex-rs";
  version = "0.1.0";
  cargoLock.lockFile = ./Cargo.lock;
  doCheck = false;
  src = ./.;
  nativeBuildInputs = [
    pkg-config
    openssl
  ];

  preBuild = lib.optionalString stdenv.hostPlatform.isLinux ''
    export CODEX_BWRAP_SHA256="$(sha256sum ${bubblewrap}/bin/bwrap | cut -d' ' -f1)"
  '';
  postInstall = lib.optionalString stdenv.hostPlatform.isLinux ''
    mkdir -p "$out/bin/codex-resources"
    ln -s ${bubblewrap}/bin/bwrap "$out/bin/codex-resources/bwrap"
  '';

  cargoLock.outputHashes = {
    "ratatui-0.29.0" = "sha256-HBvT5c8GsiCxMffNjJGLmHnvG77A6cqEL+1ARurBXho=";
    "crossterm-0.28.1" = "sha256-6qCtfSMuXACKFb9ATID39XyFDIEMFDmbx6SSmNe+728=";
    "nucleo-0.5.0" = "sha256-Hm4SxtTSBrcWpXrtSqeO0TACbUxq3gizg1zD/6Yw/sI=";
    "nucleo-matcher-0.3.1" = "sha256-Hm4SxtTSBrcWpXrtSqeO0TACbUxq3gizg1zD/6Yw/sI=";
    "runfiles-0.1.0" = "sha256-uJpVLcQh8wWZA3GPv9D8Nt43EOirajfDJ7eq/FB+tek=";
    "tokio-tungstenite-0.28.0" = "sha256-vJZ3S41gHtRt4UAODsjAoSCaTksgzCALiBmbWgyDCi8=";
    "tungstenite-0.28.0" = "sha256-CyXZp58zGlUhEor7WItjQoS499IoSP55uWqr++ia+0A=";
  };

  meta = with lib; {
    description = "OpenAI Codex command‑line interface rust implementation";
    license = licenses.asl20;
    homepage = "https://github.com/openai/codex";
  };
})
