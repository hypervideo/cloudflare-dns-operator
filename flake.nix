{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;

        name = "cloudflare-dns-operator";

        cloudflare-dns-operator = pkgs.rustPlatform.buildRustPackage {
          pname = name;
          version = "0.1.0";

          cargoHash = "sha256-d2/RG2ZHvxhFkkUQFwJDLwhWjp8E27Hq4Nm9WlqWhY4=";
          src = ./.;

          meta = with lib; {
            homepage = "https://github.com/hypervideo/cloudflare-dns-operator";
            description = "This is a kubernetes operator to manage cloudflare DNS entries from within kubernetes.";
            mainProgram = "cloudflare-dns-operator";
            maintainers = with maintainers; [ rksm ];
          };
        };

        base-image = pkgs.dockerTools.buildImage {
          name = "${name}-base";
          extraCommands = ''
            mkdir -p tmp
          '';
          copyToRoot = pkgs.buildEnv {
            name = "image-root";
            paths = with pkgs; [
              cacert
              coreutils
              bashInteractive
            ];
            pathsToLink = [ "/bin" ];
          };
        };

        image = pkgs.dockerTools.buildImage {
          inherit name;
          tag = "latest";
          created = "now";
          fromImage = base-image;
          copyToRoot = pkgs.buildEnv {
            name = "image-root";
            paths = [ cloudflare-dns-operator ];
            pathsToLink = [ "/bin" ];
          };
          config = {
            Cmd = [ "/bin/${name}" "controller" ];
          };
        };

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

        packages = { inherit cloudflare-dns-operator image; };
      }
    );
}
