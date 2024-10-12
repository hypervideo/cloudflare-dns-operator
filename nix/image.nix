{ dockerTools
, buildEnv
, cacert
, coreutils
, bashInteractive
, cloudflare-dns-operator
, name ? "cloudflare-dns-operator"
}:

let
  base-image = dockerTools.buildImage {
    name = "${name}-base";
    extraCommands = ''
      mkdir -p tmp
    '';
    copyToRoot = buildEnv {
      name = "image-root";
      paths = [
        cacert
        coreutils
        bashInteractive
      ];
      pathsToLink = [ "/bin" ];
    };
  };

in

dockerTools.buildImage {
  inherit name;
  tag = "latest";
  created = "now";
  fromImage = base-image;
  copyToRoot = buildEnv {
    name = "image-root";
    paths = [ cloudflare-dns-operator ];
    pathsToLink = [ "/bin" ];
  };
  config = {
    Cmd = [ "/bin/${name}" "controller" ];
  };
}
