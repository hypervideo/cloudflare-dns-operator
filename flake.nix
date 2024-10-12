{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        lib = pkgs.lib;
        nix = import ./nix { inherit pkgs; };
        nightly-toolchain = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);

      in
      {
        devShells = {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              rustc
              cargo
              clippy
              clang
              pkg-config
            ];

            buildInputs = with pkgs; [
              openssl
            ];

            packages = with pkgs; [
              rust-analyzer
              (rustfmt.override { asNightly = true; })
              semver-tool
              cargo-nextest
            ];

            RUST_BACKTRACE = "1";
            RUST_LOG = "debug,cloudflare=trace,hyper_util=info,tower=info,rustls=info,kube=info";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };

          nightly = pkgs.mkShell {
            packages = with pkgs; [
              cargo-udeps
              nightly-toolchain
            ];

            # cargo-udeps needs system libraries
            LD_LIBRARY_PATH = "${lib.makeLibraryPath [ pkgs.openssl pkgs.zlib ]}";
            buildInputs = lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
              pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
            ];
          };

        };

        inherit (nix) packages;
      }
    );
}
