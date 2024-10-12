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

# -=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-

IMAGE_NAME := "cloudflare-dns-operator"
DOCKER_REGISTRY := "robertkrahn"

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
