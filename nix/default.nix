{ pkgs }:

let
  cloudflare-dns-operator = pkgs.callPackage ./package.nix { };
  image = pkgs.callPackage ./image.nix { inherit cloudflare-dns-operator; };
in
{
  packages = {
    inherit cloudflare-dns-operator image;
  };
}

