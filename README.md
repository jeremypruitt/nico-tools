# nico-tools

Diagnostic CLI tooling for NICO/carbide/NCX installations. Read-only — never modifies cluster state.

## Install

```bash
# macOS / Linux one-liner (installs to /usr/local/bin):
curl -fsSL https://raw.githubusercontent.com/jeremypruitt/nico-tools/main/scripts/install.sh | bash

# Custom install directory:
INSTALL_DIR=~/.local/bin curl -fsSL https://raw.githubusercontent.com/jeremypruitt/nico-tools/main/scripts/install.sh | bash
```

Or download a tarball directly from [GitHub Releases](https://github.com/jeremypruitt/nico-tools/releases/latest) and extract manually:

```bash
# Example: Linux x86_64
tar -xzf nico-tools-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo mv nico-doctor nico-correlate /usr/local/bin/
```

Supported platforms: macOS arm64, macOS x86_64, Linux x86_64, Linux arm64.

---

## Testing against a live cluster

### Prerequisites

- A running NICO/carbide cluster accessible via kubeconfig.
- `jq` installed (for the JSON parse check; optional but recommended).
- `cargo` installed (or set `NICO_BIN_DIR` to point at pre-built binaries).

### Setup

```bash
cp .env.example .env.local
# Edit .env.local with your cluster values.
# At minimum, set KUBECONFIG and SMOKE_WORKFLOW_ID (a known recent workflow UUID or hp-* ID).
# If port-forward auto-detect works (requires kubeconfig), you can leave the URL vars unset.
```

### Run the smoke test

```bash
make smoke
# or directly:
./scripts/smoke.sh
# or with a custom env file:
./scripts/smoke.sh --env /path/to/other.env
```

The script:
1. Sources your env file.
2. Builds `nico-doctor` and `nico-correlate` (or uses `NICO_BIN_DIR` pre-built binaries).
3. Runs `nico-doctor` — asserts exit code is 0 (ok), 1 (warn), or 2 (fail), **not** 3 (internal error).
4. Runs `nico-correlate $SMOKE_WORKFLOW_ID` — asserts exit code is 0 (found) or 2 (no data), **not** 1 (ID not found).
5. Runs `nico-doctor --json | jq .` — asserts the JSON output parses cleanly.
6. Prints a pass/fail summary with timing.

### Quick dev runs

```bash
# Run nico-doctor with any extra flags via ARGS=
make run-doctor
make run-doctor ARGS="--skip postgres,loki"
make run-doctor ARGS="--json"

# Run nico-correlate against a specific entity
make run-correlate ARGS="host-r12u5"
make run-correlate ARGS="--json some-workflow-id"
```

### Reach mode and env vars

After #31 lands (auto-detect reach mode), the script relies on port-forward auto-detect when
`NICO_REACH_MODE` is unset and `KUBERNETES_SERVICE_HOST` is absent. Explicit URL env vars
(`NICO_TEMPORAL_ADDRESS`, `NICO_POSTGRES_URL`, `LOKI_URL`) still override auto-detect, so you
can point the tools at already-forwarded ports without any Kubernetes access.

---

## Configuration file

Both `nico-doctor` and `nico-correlate` load `~/.config/nico-tools/config.toml` automatically if it exists. Write it once and stop juggling environment variables every session.

**Precedence:** CLI flag > environment variable > config file > built-in default

```toml
# ~/.config/nico-tools/config.toml

[cluster]
namespace = "nico"
context   = "my-cluster"      # optional — omit to use current kubeconfig context

[temporal]
address          = "localhost:7233"
namespace        = "default"
stuck_threshold  = "30m"

[postgres]
url = "postgres://nico:secret@db:5432/nico"

[output]
color  = "auto"   # auto | always | never
format = "human"  # human | json
```

Use `--config <path>` on either binary to load a different file:

```bash
nico-doctor --config /etc/nico/config.toml
nico-correlate --config ~/my-cluster.toml host-r12u5
```

Environment variables (`NICO_TEMPORAL_ADDRESS`, `NICO_POSTGRES_URL`, `NICO_NAMESPACE`, etc.) still work and override the config file. CLI flags override everything.

---

## Crates

| Crate | Binary | Purpose |
|-------|--------|---------|
| `nico-doctor` | `nico-doctor` | Six-layer cluster health check |
| `nico-correlate` | `nico-correlate` | Cross-source event correlation for a single entity |
| `nico-common` | — | Shared types |

## nico-correlate

Aggregates events and current state from every available source (Temporal, Postgres, k8s, Loki, Redfish) into a unified timeline for a given entity ID. Each source is independently optional — if one is unreachable the rest still run.

### Build

```bash
cargo build --release -p nico-correlate
# Binary at: target/release/nico-correlate
```

### Quick start

```bash
# Host entity
NICO_POSTGRES_URL=postgres://nico:secret@localhost:5432/nico nico-correlate host-r12u5

# DPU entity (resolves via hosts.dpu_id)
NICO_POSTGRES_URL=postgres://nico:secret@localhost:5432/nico nico-correlate dpu-bf3-r12u5

# Postgres only
NICO_POSTGRES_URL=... nico-correlate --sources postgres host-r12u5

# JSON output for scripting
NICO_POSTGRES_URL=... nico-correlate --json host-r12u5
```

---

## Using nico-correlate against a NICO/carbide Postgres database

### Constructing NICO_POSTGRES_URL

`NICO_POSTGRES_URL` is a standard libpq connection string. The database runs inside the carbide cluster; use `kubectl port-forward` to reach it from your workstation.

**Step 1 — find the Postgres credentials secret**

```bash
kubectl get secret -n nico-system -l app.kubernetes.io/component=postgresql -o name
# e.g. secret/nico-postgresql
```

**Step 2 — extract the DSN fields**

```bash
kubectl get secret nico-postgresql -n nico-system -o jsonpath='{.data.postgres-password}' | base64 -d
# prints the password; default user is usually "nico" and database "nico"
```

Alternatively, if the secret stores a full DSN key:

```bash
kubectl get secret nico-postgresql -n nico-system -o jsonpath='{.data.database-url}' | base64 -d
```

**Step 3 — port-forward Postgres, then run the tool**

```bash
# Terminal 1: forward the Postgres port
kubectl port-forward -n nico-system svc/nico-postgresql 5432:5432

# Terminal 2: run nico-correlate against it
NICO_POSTGRES_URL="postgres://nico:<password>@localhost:5432/nico" \
  nico-correlate host-r12u5
```

One-liner (background port-forward, run tool, then clean up):

```bash
kubectl port-forward -n nico-system svc/nico-postgresql 5432:5432 &
PF_PID=$!
NICO_POSTGRES_URL="postgres://nico:<password>@localhost:5432/nico" \
  nico-correlate host-r12u5
kill $PF_PID
```

> `NICO_POSTGRES_URL` is the only required input. No config file is needed.

---

### Typical operator queries

| Goal | Command |
|------|---------|
| Look up a host by ID | `NICO_POSTGRES_URL=... nico-correlate host-r12u5` |
| Look up a DPU (resolves via `hosts.dpu_id`) | `NICO_POSTGRES_URL=... nico-correlate dpu-bf3-r12u5` |
| Scope to Postgres only | `NICO_POSTGRES_URL=... nico-correlate --sources postgres host-r12u5` |
| JSON output for scripting | `NICO_POSTGRES_URL=... nico-correlate --json host-r12u5` |
| Workflow correlation | `NICO_POSTGRES_URL=... nico-correlate <workflow-id>` |
| Limit look-back window | `NICO_POSTGRES_URL=... nico-correlate --since 30m host-r12u5` |

---

### Tables queried per entity type

| Entity type | ID prefix example | Tables queried |
|-------------|------------------|---------------|
| Host | `host-r12u5` | `hosts` (WHERE `id = ?`), `audit_log` |
| DPU | `dpu-bf3-r12u5` | `hosts` (WHERE `dpu_id = ?`), `audit_log` |
| Workflow | `<uuid>` or `hp-*` | `workflows` (WHERE `id = ?`), `audit_log` |
| Request | `req-*` | `audit_log` only |

The `audit_log` table is queried for all entity types using `entity_id = ?`, returning the 100 most recent events ordered by `ts DESC`.

---

### Expected output shape

**Human-readable (default):**

```
detected type: host
Timeline (3 events):
  14:02:11  postgres  create_host
  14:03:45  postgres  provision_start
  14:08:22  postgres  provision_complete

Postgres state (current):
  hosts.id: host-r12u5
  hosts.status: ready
  hosts.dpu_id: dpu-bf3-r12u5
  hosts.created_at: 2026-05-04T14:02:11Z
[source unavailable: temporal]
[source unavailable: loki]
```

**JSON (`--json`):**

```json
{
  "version": 1,
  "id": "host-r12u5",
  "id_type": "host",
  "events": [
    {
      "ts": "2026-05-04T14:02:11Z",
      "source": "postgres",
      "kind": "create_host",
      "severity": "info"
    }
  ],
  "sources_unavailable": ["temporal", "loki"],
  "state": [
    { "source": "postgres", "key": "hosts.id",     "value": "host-r12u5" },
    { "source": "postgres", "key": "hosts.status", "value": "ready" }
  ]
}
```

---

### When the database is unreachable

If `NICO_POSTGRES_URL` is not set or the connection fails, the Postgres source reports itself unavailable and the tool continues with the remaining sources. The output line looks like:

```
[source unavailable: postgres]
```

No crash. Exit code is 1 (partial data) rather than 2 (no data) if at least one other source returned results.

To confirm connectivity before running:

```bash
psql "$NICO_POSTGRES_URL" -c "SELECT 1"
```
