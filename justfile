default:
    just --list

install-crds:
    cargo run -q -- crds > crds.yaml
    kubectl create -f crds.yaml

delete-crds:
    # kubectl delete crds hyperservers.hyper.video
    kubectl delete -f crds.yaml

run *args="":
    cargo run -- {{ args }}
