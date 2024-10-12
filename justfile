default:
    just --list

gen-crds:
    cargo run -q -- crds > crds.yaml

install-crds: gen-crds
    kubectl create -f crds.yaml

delete-crds:
    kubectl delete -f crds.yaml

run *args="":
    cargo run -- {{ args }}
