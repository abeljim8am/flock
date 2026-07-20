{ pkgs, ... }:

{
  # Keep devenv's Rust toolchain in lockstep with the repository. This imports
  # the pinned compiler, rustfmt/clippy components, and WASI/musl targets.
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  packages = with pkgs; [
    # General repository tooling.
    git
    curl

    # Native dependencies used by the main build and test-only crates.
    pkg-config
    openssl
    protobuf

    # Build tools used by vendored curl, libssh2, and OpenSSL crates.
    cmake
    gnumake
    perl
  ];

  enterTest = ''
    rustc --version | grep --fixed-strings 'rustc 1.92.0'
    cargo --version
    rustfmt --version
    cargo clippy --version
    protoc --version
    pkg-config --exists openssl
    cargo metadata --no-deps --format-version 1 >/dev/null
  '';
}
