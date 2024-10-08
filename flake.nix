{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let pkgs = import nixpkgs { inherit system; };
      in
        {
          devShells.default = pkgs.mkShell {
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
            ];

            RUST_BACKTRACE = "1";
            RUST_LOG = "debug,hyper_util=info,tower=info,rustls=info,kube=info";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };
        }
    );
}
