default:
    just --list

gen-crds:
    cargo run -q -- crds > crds.yaml

install-crds: gen-crds
    kubectl create -f crds.yaml

delete-crds:
    kubectl delete -f crds.yaml

version:
    @cargo build -q
    @./target/debug/cloudflare-dns-operator --version | cut -d' ' -f2

run *args="":
    cargo run -- {{ args }}

controller:
    CHECK_DNS_RESOLUTION="30s" just run controller

test:
    cargo nextest run

udeps:
    CARGO_TARGET_DIR="./target/udeps" nix develop .#nightly -c cargo udeps

# -=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-

IMAGE_NAME := "cloudflare-dns-operator"
DOCKER_REGISTRY := "robertkrahn"

bump-version:
    #!/usr/bin/env sh
    set -e
    old_version=$(just version)
    new_version=$(semver bump patch $old_version)
    echo "Bumping version to $new_version"
    sed -i "s/^version = \"$old_version\"/version = \"$new_version\"/" Cargo.toml
    sed -i "s/version = \"$old_version\"/version = \"$new_version\"/" nix/package.nix

docker-build:
    nix build '.#image' |& nom
    docker load < ./result
    rm result

docker-push VERSION=`just version`: docker-build
    docker tag {{ IMAGE_NAME }}:latest {{ IMAGE_NAME }}:{{ VERSION }}
    docker tag {{ IMAGE_NAME }}:{{ VERSION }} {{ DOCKER_REGISTRY }}/{{ IMAGE_NAME }}:{{ VERSION }}
    docker tag {{ IMAGE_NAME }}:{{ VERSION }} {{ DOCKER_REGISTRY }}/{{ IMAGE_NAME }}:latest
    docker push {{ DOCKER_REGISTRY }}/{{ IMAGE_NAME }}:{{ VERSION }}
    docker push {{ DOCKER_REGISTRY }}/{{ IMAGE_NAME }}:latest

docker-run:
    docker run --rm -it {{ IMAGE_NAME }}:latest bash
