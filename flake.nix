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
        };
        nativeBuildInputs = with pkgs;[
          pkg-config
          llvmPackages.clang
          libopus
        ];
        buildInputs = with pkgs; [
          openssl
          pipewire
        ];
        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
      in
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = "test-rust-cast";
          version = "local";
          src = pkgs.lib.cleanSource ./.;
          inherit nativeBuildInputs buildInputs LIBCLANG_PATH;
          cargoLock.lockFile = ./Cargo.lock;
        };
        devShell = pkgs.mkShell {
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
