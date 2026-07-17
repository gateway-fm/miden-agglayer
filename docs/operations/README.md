# Operations

These documents describe the service implemented by the current `main` branch.
They are deployment-neutral: discover names, namespaces, secret sources, and
database endpoints from the deployment you are operating instead of copying a
historical cluster value.

| Document | Purpose |
|---|---|
| [Runbook](runbook.md) | Production constraints, startup, safe shutdown, recovery choices, and incident procedures |
| [Monitoring](monitoring.md) | Health/metrics endpoints, high-signal metrics, alerts, and dashboards |
| [Diagnostics](diagnostics.md) | Read-only collection and symptom-to-cause investigation |
| [Quarantine](quarantine.md) | Handling B2AGG exits the projector deliberately cannot emit |
| [Upgrade guide](../UPGRADE.md) | Version-neutral in-place upgrade and rollback procedure |

Architecture and data-flow context is in
[the architecture guide](../ARCHITECTURE.md).

## Conventions

- Commands use variables such as `$NAMESPACE`, `$WORKLOAD`, `$POD`,
  `$PROXY_RPC`, and `$DATABASE_URL`. Resolve them from the live deployment
  first; the examples do not assume Kubernetes object names.
- A command that needs deployment-specific authority states that prerequisite
  in prose instead of presenting an incomplete command.
- Start with read-only diagnostics. Database writes, account initialization,
  store reset, restore, scaling, and image changes are recovery actions and
  require an approved change/incident procedure.
- Never expose port 8546 directly to the public internet. Bind privately or put
  it behind an authenticated, rate-limited network boundary.
- Never run two service processes against one Miden store. The projector and
  sqlite client are single-owner components.
- Never delete or replace `keystore/` or `bridge_accounts.toml` as part of a
  routine recovery.

## Discover a Kubernetes deployment

The repository does not contain the production cluster manifest, so discover
the live objects instead of relying on names from an old runbook:

```bash
kubectl config current-context
kubectl get namespaces
kubectl -n "$NAMESPACE" get deploy,statefulset,pod,service \
  -o wide | grep -i miden
kubectl -n "$NAMESPACE" get pod "$POD" \
  -o jsonpath='{.spec.containers[*].name}{"\n"}'
kubectl -n "$NAMESPACE" get pod "$POD" \
  -o jsonpath='{.spec.containers[*].image}{"\n"}'
```

Resolve secret *names* and environment wiring from the workload manifest. Do
not print secret values into terminals captured by support tooling:

```bash
kubectl -n "$NAMESPACE" get "$WORKLOAD" -o yaml
```

Use the workload kind that actually exists. If neither command identifies the
service unambiguously, stop and obtain the deployment inventory from its owner
before performing recovery actions.
