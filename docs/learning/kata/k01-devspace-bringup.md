# k01 — devspace bringup (core-only)

**Status:** seed
**Deployment-type produced:** `core-only` (carbide-kind territory; no REST, no Temporal)
**Hooks into nico:** boot probe + deployment-type detection on a real local cluster. Unblocks k02, k03, k05, k06. Does **not** unblock k04 (needs full chain) or k07 (needs Temporal) — those wait on a future `k01c — full bringup` kata.

## Why this kata exists

`infra-controller-core/devspace.yaml` is the cheapest, most-canonical local NICo bringup. It produces the `core-only` deployment-type (per CONTEXT.md "Deployment-type" and PRD-001), which is enough to exercise the per-DPU layers (`hbn`, `dpu_cert`, `dpu_isolation`, `dpu_health`, `dpu_services`) and the fleet-wide `dpu` layer's forgedb-backed sub-checks. That's most of what shipped in PRDs 002–004.

What `core-only` does **not** give you:
- No `carbide-rest` → no end-to-end REST→core→site-agent→DPU-agent trace (k04).
- No Temporal → `workflows` layer skips via the `temporal_present` gate (k07).
- No real hardware → per-DPU layers will headline `not-yet-known` / `no machines row` until you seed forgedb (open question below).

For those gaps we'll spec sibling kata later:
- `k01b` — rest-only-mock via `infra-controller-rest`'s `make kind-reset` (REST + mock-core, no forgedb).
- `k01c` — full (both stacks side-by-side, custom plumbing — no canonical `make` target exists today).

## Prereqs

- `../infra-controller-core` cloned as a sibling (`git remote -v` shows `NVIDIA/infra-controller-core`).
- Docker, `kubectl`, `helm`, `devspace` on `$PATH`.
- A clean kubeconfig context — devspace will create/select one.
- This repo built: `cargo build -p nico` so `nico doctor` is ready.

## Steps

1. Read `../infra-controller-core/devspace.yaml` and `../infra-controller-core/docs/getting-started/` end-to-end first. Don't run anything yet. Note the namespace (`forge-system` by default per the `LOCAL_DEV_NAMESPACE` var) and the helm prereq layer (`../infra-controller-core/helm-prereqs/` — keycloak, vault, postgres operators).
2. Bring it up. Capture every command run — including the ones that failed and how you fixed them. The "what wasn't obvious" list is the actual deliverable, not the running cluster.
3. Confirm pods are `Running` in `forge-system`. Expected components from the helm chart: `carbide-api`, `carbide-bmc-proxy`, `carbide-dns`, `carbide-dhcp`, `carbide-hardware-health`, `carbide-pxe`, `carbide-ssh-console-rs`, `carbide-dsx-exchange-consumer`, `unbound`. Plus prereqs from `helm-prereqs/` (keycloak, vault, postgres). **Not** expected: `carbide-rest`, `temporal-frontend`, `site-agent`.
4. Set `KUBECONFIG` to whatever devspace produced, then run `nico doctor` from this repo with no flags. Capture:
   - The boot probe output (connecting / validating / serving sections).
   - The deployment-type line in the banner: `· type: <name> (<source>)`. Should resolve to `core-only` via `auto`. If it doesn't, that's a detection bug — file it.
   - The exit code (0/1/2 expected; 3 = boot probe failed and we have a real bug to file).
5. Run `nico doctor --json | jq '.layers[] | {name, status}'` and check that:
   - `workflows` layer is `skipped` (Temporal absent — `temporal_present` gate).
   - Per-DPU + `dpu` fleet layers ran (forgedb is present in `core-only`) and headline whatever the empty `machines` table allows (`no-recent-status` / `not-yet-known` / `no machines row` — these are correct, not bugs, with no enrolled hardware).

## Capture (fill in as you go)

- Detected deployment-type + source: _________________
- Layers that ran: _________________
- Layers that skipped + reason: _________________
- Pods in `forge-system` (one-line summary): _________________
- Things that weren't obvious from the README:
  - _________________
  - _________________
  - _________________

## Definition of done

- `nico doctor` exits 0/1/2 against the cluster.
- Banner shows `type: core-only (auto)`.
- `workflows` is `skipped`; per-DPU layers ran and headlined the empty-state verdicts.
- "Things that weren't obvious" has at least three entries.
- `kata/INDEX.md` k01 row flipped to `done`.

## Open questions to capture, not solve here

- Is there a way to seed a fake `machines` row in devspace's forgedb so per-DPU layers exercise their non-degenerate path? Without it, k05/k06 can read the schema but can't see a green `dpu-cert` verdict — they only ever see the empty-state branch. The right place to put that seeder is probably alongside `dev/cleanup_bootstrap.sql`.
- Does core devspace deploy postgres directly, or does it rely on a postgres operator from `helm-prereqs/`? Affects what `nico doctor` sees in the `postgres` layer and how to point it at forgedb.
- Does carbide-vault in `helm-prereqs/` issue real client certs to anything in this empty-cluster shape? If yes, k06 (cert expiry) is unblocked on `core-only`. If no, k06 has to wait for a hardware-attached deployment.
