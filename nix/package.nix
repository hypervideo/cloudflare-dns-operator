{ lib
, rustPlatform
, name ? "cloudflare-dns-operator"
}:

rustPlatform.buildRustPackage {
  version = "0.1.14";
  pname = name;

  cargoHash = "sha256-npO7gJnKh7wakd8oePCmbPH2DUtqhSUusEpDc2Dbh3M=";
  src = ../.;

  meta = with lib; {
    homepage = "https://github.com/hypervideo/cloudflare-dns-operator";
    description = "This is a kubernetes operator to manage cloudflare DNS entries from within kubernetes.";
    mainProgram = "cloudflare-dns-operator";
    license = licenses.mpl20;
    maintainers = with maintainers; [ rksm ];
  };
}
