# cloudflare-dns-operator

[This is a kubernetes operator](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/) (custom resource definition + kubernetes controller) to manage cloudflare DNS entries from within kubernetes using the cloudflare API.

__Note:__ This is an unofficial project and not affiliated in any way with cloudflare.

## Installation

In your kubernetes cluster install the [`crds.yaml`](./crds.yaml) file and a deployment matching [examples/deployment.yaml](./examples/deployment.yaml).

This sets up the controller as a deployment. It'll watch for `CloudflareDNSRecord` resources and create/update/delete DNS records in cloudflare.

You can then create a new DNS record like this:

``` yaml
apiVersion: dns.cloudflare.com/v1alpha1
kind: CloudflareDNSRecord
metadata:
  name: my-cloudflare-dns-record
spec:
  name: example.com
  type: A
  ttl: 3600
  content: "1.2.3.4"
  zoneId:
    from:
      secret:
        name: cloudflare-dns-secret
        key: zone-id
  apiToken:
    from:
      secret:
        name: cloudflare-dns-secret
        key: api-token
  comment: "Managed by the Cloudflare DNS Operator"
  tags:
    - k8s
```

You can also automatically expose LoadBalancer services or external IP services by referencing a service in the `content` instead of a static IP:

``` yaml
# ...
  content:
    service:
      name: traefik
      namespace: traefik
# ...
```

API token and zone ID can also be set verbatim (not recommended):

``` yaml
# ...
  zoneId:
    value: "1234567890abcdef1234567890abcdef"
# ...
```

