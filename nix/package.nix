{ lib
, rustPlatform
, name ? "cloudflare-dns-operator"
}:

rustPlatform.buildRustPackage {
  version = "0.1.19";
  pname = name;

  cargoHash = "sha256-lIQ5+iPsgWtmLAKyBxa6lth05aRgIfjXM0L0oI/G3IU=";
  src = ../.;

  meta = with lib; {
    homepage = "https://github.com/hypervideo/cloudflare-dns-operator";
    description = "This is a kubernetes operator to manage cloudflare DNS entries from within kubernetes.";
    mainProgram = "cloudflare-dns-operator";
    license = licenses.mpl20;
    maintainers = with maintainers; [ rksm ];
  };
}
