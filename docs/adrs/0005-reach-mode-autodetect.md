# ADR-005: Reach mode — auto-detect port-forward vs. in-cluster

- **Status:** Accepted
- **Date:** 2026-05-03

## Context

`nico-doctor` needs to reach service endpoints (`/healthz`, `/readyz`, gRPC
reflection, Postgres). It runs in two contexts: locally on an operator's
laptop with kubeconfig access, and inside the cluster (e.g., a debug pod or a
CI runner with Pod Identity).

## Decision

Two reach modes, auto-detected:

- **`port-forward`** — opens a `kube` port-forward in-process for each
  service it needs to hit, calls localhost, closes the forward. Slower but
  works from anywhere with kubeconfig access.
- **`in-cluster`** — uses cluster DNS directly
  (`<service>.<namespace>.svc.cluster.local`). Faster.

Auto-detection: if `KUBERNETES_SERVICE_HOST` is set in the environment, use
`in-cluster`. Otherwise use `port-forward`.

This can be overridden with `--mode port-forward` or `--mode in-cluster` for
testing or unusual environments.

## Consequences

### Positive
- "Just works" from a laptop or a debug pod — no flags needed for the common
  case.
- Same binary, same code path; no separate "local" and "cluster" builds.
- Override exists for edge cases (e.g., running locally but tunneling through
  Tailscale to cluster DNS).

### Negative / Trade-offs
- Port-forward mode is meaningfully slower (each forward has setup/teardown
  overhead).
- Port-forward mode requires the kubeconfig user to have `pods/portforward`
  permission on the target namespace.

## Alternatives Considered

- **Always port-forward:** rejected. In-cluster usage (CI, debug pods) doesn't
  need it and pays the latency cost for nothing.
- **Always in-cluster:** rejected. Doesn't work from a laptop without a
  sidecar.
- **Manual flag, no auto-detect:** rejected. The detection rule is
  unambiguous (`KUBERNETES_SERVICE_HOST` is set iff we're in a pod) so the
  default-on auto-detection costs nothing and saves a flag.

## Related

- ADR-006 (concurrency) — port-forward setup is part of what the per-check
  timeout has to accommodate.
