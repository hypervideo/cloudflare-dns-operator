# cloudflare-dns-operator

[This is a kubernetes operator](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/) (custom resource definition + kubernetes controller) to manage cloudflare DNS entries from within kubernetes using the cloudflare API.

__Note:__ This is an unofficial project and not affiliated in any way with cloudflare.

## Installation

In your kubernetes cluster install the [`crds.yaml`](./crds.yaml) file and a deployment matching [examples/deployment.yaml](./examples/deployment.yaml). Note that you'll need to set the env var `CLOUDFLARE_API_TOKEN` to a valid cloudflare API token.

This sets up the controller as a deployment. It'll watch for `CloudflareDNSRecord` resources and create/update/delete DNS records in cloudflare.

You can optionally have the controller check the records by doing DNS lookups from 1.1.1.1. The resolution result will be reflected in the `status.pending` field of the `CloudflareDNSRecord` resource. For this to be enabled, set the env var `CHECK_DNS_RESOLUTION` to a human readable duration like `5m` or `1h` or `60s`.

You can then create a new DNS record like this:

``` yaml
apiVersion: dns.cloudflare.com/v1alpha1
kind: CloudflareDNSRecord
metadata:
  name: my-cloudflare-dns-record
spec:
  name: foo.example.com
  type: A
  ttl: 3600
  content: "1.2.3.4"
  zone:
    name:
      value: example.com
  comment: "Managed by the Cloudflare DNS Operator"
  tags:
    - k8s
```

You can also automatically expose IPs from LoadBalancer services or external IP services by referencing a service in the `content` instead of a static IP:

``` yaml
# ...
  content:
    service:
      name: traefik
      namespace: traefik
# ...
```

The zone can also be set with a `secret` or `configMap` reference like this:

``` yaml
# ...
  zone:
    name:
      from:
        secret:
          name: cloudflare-dns-secret
          key: zone-name
# ...
```

