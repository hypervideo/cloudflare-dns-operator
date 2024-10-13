{ lib
, rustPlatform
, name ? "cloudflare-dns-operator"
}:

rustPlatform.buildRustPackage {
  version = "0.1.4";
  pname = name;

  cargoHash = "sha256-fgr27EfXBsuCGXzLY29uJrs9FGI77oQkFiKEgAV8Olk=";
  src = ../.;

  meta = with lib; {
    homepage = "https://github.com/hypervideo/cloudflare-dns-operator";
    description = "This is a kubernetes operator to manage cloudflare DNS entries from within kubernetes.";
    mainProgram = "cloudflare-dns-operator";
    license = licenses.mpl20;
    maintainers = with maintainers; [ rksm ];
  };
}
