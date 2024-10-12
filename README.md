# cloudflare-dns-operator

[This is a kubernetes operator](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/) (custom resource definition + kubernetes controller) to manage cloudflare DNS entries from within kubernetes.

## Installation

In your kubernetes cluster install the `crds.yaml` file and a deployment matching the following:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: cloudflare-dns-operator
spec:
  replicas: 1
  selector:
    matchLabels:
      app: cloudflare-dns-operator
  template:
    metadata:
      labels:
        app: cloudflare-dns-operator
    spec:
      containers:
      - name: cloudflare-dns-operator
        image: robertkrahn/cloudflare-dns-operator:latest
        env:
        - name: RUST_LOG
          value: info
```
