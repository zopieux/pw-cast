{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        rust = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" ];
        };
        rustPlatform = pkgs.makeRustPlatform {
          rustc = rust;
          cargo = rust;
          stdenv = pkgs.clang12Stdenv;
        };
        nativeBuildInputs = with pkgs; [
          pkg-config
        ];
        buildInputs = with pkgs; [
          openssl
          pipewire
        ];
        LIBCLANG_PATH = "${pkgs.llvmPackages_12.libclang.lib}/lib";
      in
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = "pw-cast";
          version = "local";
          src = pkgs.lib.cleanSource ./.;
          inherit nativeBuildInputs buildInputs LIBCLANG_PATH;
          cargoLock.lockFile = ./Cargo.lock;
          cargoLock.outputHashes = {
            "cast-sender-0.2.0" = "sha256-7pMvE3LW6npOUhsq8gy2OTougXFXBm9ip8g/GtzkewM=";
            "libspa-0.8.0" = "sha256-p/wrcNf+jBMeAGFxUzaOBfC3xxuymPspJzoTj0EFCr8=";
          };
        };
        devShell = pkgs.mkShell.override { stdenv = pkgs.clang12Stdenv; } {
          inherit nativeBuildInputs LIBCLANG_PATH;
          buildInputs = buildInputs ++ [
            rust
            pkgs.cargo-edit
            pkgs.cargo-watch
          ];
        };
      }
    );
}
