{ lib
, rustPlatform
, name ? "cloudflare-dns-operator"
}:

rustPlatform.buildRustPackage {
  version = "0.1.2";
  pname = name;

  cargoHash = "sha256-XrHr2E/RyJr/mzzhTSGouA9uThZaQtj5IxKBKjbqwH8=";
  src = ../.;

  meta = with lib; {
    homepage = "https://github.com/hypervideo/cloudflare-dns-operator";
    description = "This is a kubernetes operator to manage cloudflare DNS entries from within kubernetes.";
    mainProgram = "cloudflare-dns-operator";
    maintainers = with maintainers; [ rksm ];
  };
}
