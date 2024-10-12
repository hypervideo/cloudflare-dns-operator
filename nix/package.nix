{ lib
, rustPlatform
, name ? "cloudflare-dns-operator"
}:

rustPlatform.buildRustPackage {
  version = "0.1.1";
  pname = name;

  cargoHash = "sha256-o9kVVmF3jQ7wJz4InCgCucgyPcyP29kv2LFAt8pcu2E=";
  src = ../.;

  meta = with lib; {
    homepage = "https://github.com/hypervideo/cloudflare-dns-operator";
    description = "This is a kubernetes operator to manage cloudflare DNS entries from within kubernetes.";
    mainProgram = "cloudflare-dns-operator";
    maintainers = with maintainers; [ rksm ];
  };
}
