# nico-doctor & nico-correlate — Design
Two Rust binaries that compress the diagnostic ladder into seconds. Designed to be readable at a glance, scriptable for CI, and to never hide the underlying tools — every output line points at where to dig deeper.
---
## Shared Foundations
Before either tool, a tiny shared crate. Same patterns, same look, same config.
### Workspace layout

nico-tools/
├── Cargo.toml                 # workspace
├── crates/
│   ├── nico-common/           # shared: config, output, k8s/temporal/pg clients
│   ├── nico-doctor/           # binary 1
│   └── nico-correlate/        # binary 2

Workspace Cargo.toml pins versions; both binaries depend on nico-common. This avoids two copies of every client and keeps the visual language consistent.
### Core dependencies
| Crate | Why |
|---|---|
| clap (derive) | CLI parsing. Subcommands, env-var fallback, auto-generated help |
| tokio | Async runtime. Parallel I/O is the whole point of nico-doctor |
| kube + k8s-openapi | Kubernetes client. First-party, well-maintained |
| tonic + reflection | gRPC client with reflection so we don't ship .proto |
| reqwest | HTTP for /healthz, /readyz, Redfish, Temporal Web API |
| sqlx | Postgres. Compile-time-checked queries against a real DB |
| temporal-client (or tonic against Temporal's gRPC) | Workflow forensics |
| serde / serde_json | Everything is JSON-shaped |
| tracing + tracing-subscriber | Structured logging for the tools themselves |
| owo-colors or nu-ansi-term | Terminal colors. Respects NO_COLOR and --no-color |
| comfy-table | ASCII tables for the summary view |
| anyhow / thiserror | anyhow at binary boundaries, thiserror in libraries |
| directories | Config file location (XDG on Linux, etc.) |
Avoid: heavy TUI crates (ratatui, cursive). Both tools print and exit.
### Configuration
Config file at ~/.config/nico-tools/config.toml, overridable via env vars and flags. Keep it small.
toml
[cluster]
context = "kind-nico-dev"
namespace = "nico"
[postgres]
url = "postgres://nico:[nico@localhost:5432](mailto:nico@localhost:5432)/nico"
[temporal]
address = "localhost:7233"
namespace = "default"
[services]
# service name -> { port, health_path, grpc_health_method }
core    = { port = 9090, health = "/healthz", ready = "/readyz" }
rest    = { port = 8080, health = "/healthz", ready = "/readyz" }
agent   = { port = 9091, health = "/healthz", ready = "/readyz" }
scout   = { port = 9092, health = "/healthz", ready = "/readyz" }
[output]
color = "auto"     # auto | always | never
format = "human"   # human | json

Precedence: flag > env (NICO_*) > config file > sensible default. Don't ask for inputs interactively — these are scriptable tools.
### Output discipline
Three rules that make the tools feel professional:
1. **Human format fits one screen.** Default terminal is 24 lines; the summary should fit in 20 with room for headers. If you have more to say, summarize and point at --verbose or --json.
2. **JSON format is machine-complete.** --json outputs everything, structured. This is what CI hooks and your eventual correlation tool will consume.
3. **Colors are semantic, not decorative.** Green = ok, yellow = warning (degraded but functional), red = error (broken), gray = unknown/not-checked. Use sparingly.
Status icons (Unicode, with ASCII fallback for --ascii):

✓  ok      (green)
!  warn    (yellow)
✗  fail    (red)
?  unknown (gray)
·  skipped (dim)

---
## nico-doctor
A read-only health check that runs the ladder's first six steps in parallel and prints one screen.
### Interface

nico-doctor [OPTIONS]
OPTIONS:
  -n, --namespace <NS>          Kubernetes namespace [default: nico]
      --context <CTX>           Kubernetes context [env: NICO_CONTEXT]
      --skip <LAYERS>           Comma-separated layers to skip
                                  (cluster,logs,workflows,health,grpc,postgres)
      --since <DURATION>        Look-back window for logs/events [default: 10m]
      --timeout <DURATION>      Per-check timeout [default: 5s]
  -j, --json                    Output JSON
  -v, --verbose                 Show details for non-failing checks
      --ascii                   ASCII-only output (no Unicode)
      --no-color                Disable color output
  -h, --help                    Show help
  -V, --version                 Show version

Exit codes:
- 0 — all checks ok or skipped
- 1 — at least one warning, no failures
- 2 — at least one failure
- 3 — could not run checks (config error, can't reach cluster)
This is what makes it CI-usable: nico-doctor --json && [deploy.sh](http://deploy.sh) gates on health.
### What you see (default)

nico-doctor — kind-nico-dev / nico                       2026-04-30 14:22:18
  ✓ cluster      14 pods Ready, 0 restarts in last 10m, 0 warning events
  ✓ logs         no error/panic in last 10m across 14 pods
  ! workflows    1 stuck (>30m running), 0 failed in last 1h
  ✓ health       6/6 services pass /healthz and /readyz
  ✓ grpc         core reachable, 47 RPCs registered
  ! postgres     pool 18/20 in-use, 1 lock wait >5s
Summary:                                                  2 warnings, 0 failures
Stuck workflows (1):
  • HostProvisioning  workflow_id=hp-7f3a2c  running 47m
    last event: ConfigureDPU started 12m ago
    → temporal workflow show -w hp-7f3a2c
Postgres warnings:
  • Lock wait: pid 1843 waiting on AccessExclusiveLock for hosts (8.4s)
    → SELECT * FROM pg_stat_activity WHERE pid IN (1843, ...);
Hint: --verbose for details on passing checks, --json for machine output

The format is deliberate:
- One line per layer at the top — fits the eye
- Section per layer that has warnings or failures — only the interesting parts
- Every problem points at the next command to run — never a dead end
- Footer hint reminds about --verbose/--json so the muscle memory persists
### What you see (--verbose)
Same header; every layer expands with one or two lines of detail even when green. Useful when you want to confirm "yes I really did check that" and see the numbers.
### What you see (--json)
json
{
  "version": 1,
  "timestamp": "2026-04-30T14:22:18Z",
  "context": "kind-nico-dev",
  "namespace": "nico",
  "duration_ms": 1847,
  "summary": { "ok": 4, "warn": 2, "fail": 0, "unknown": 0 },
  "layers": {
    "cluster": {
      "status": "ok",
      "checks": [
        { "name": "pods_ready", "status": "ok", "value": "14/14" },
        { "name": "recent_restarts", "status": "ok", "value": 0 },
        { "name": "warning_events_10m", "status": "ok", "value": 0 }
      ]
    },
    "workflows": {
      "status": "warn",
      "findings": [
        {
          "kind": "stuck_workflow",
          "workflow_id": "hp-7f3a2c",
          "workflow_type": "HostProvisioning",
          "running_for_seconds": 2820,
          "last_event": { "type": "ActivityTaskStarted", "name": "ConfigureDPU", "ago_seconds": 720 },
          "next_command": "temporal workflow show -w hp-7f3a2c"
        }
      ]
    },
    "postgres": { ... }
  }
}

JSON is the contract. Keep it stable across versions; bump version if breaking.
### Layer implementation notes
Each layer is a function returning LayerResult. They run concurrently with tokio::join! (or FuturesUnordered if you want streaming progress). Per-check timeouts ensure one slow query doesn't stall the whole report.
| Layer | Concrete check |
|---|---|
| **cluster** | kube client: list pods in namespace, count Ready/total, scan for restartCount > 0 since --since, list Warning-type events since --since |
| **logs** | kube client: stream logs from each pod with since_seconds, count lines matching (?i)error\|panic\|fatal. Cap at e.g. 500 lines per pod to bound memory |
| **workflows** | Temporal: list workflows with ExecutionStatus="Running" and StartTime < now()-30m for stuck; list with ExecutionStatus="Failed" and CloseTime > now()-1h for failed |
| **health** | For each service: HTTP GET /healthz and /readyz via in-cluster DNS or port-forward. Need to decide which — see "How does it reach things" below |
| **grpc** | tonic + reflection against Core: list services, count methods, optionally call a known cheap method like GetServerInfo |
| **postgres** | sqlx: query pg_stat_activity for waits, query pg_stat_database for connection counts, optionally pg_locks for explicit lock waits |
### How does it reach things
Two modes, controlled by config or --mode:
- **port-forward** (default for local): for each service the tool needs to hit, open a kube port-forward in-process, hit [localhost](http://localhost), close it. Slightly slower but works from anywhere with kubeconfig access.
- **in-cluster**: assumes you're running it from inside the cluster (a debug pod, a sidecar) and uses cluster DNS directly. Faster.
Auto-detect: if KUBERNETES_SERVICE_HOST is set, use in-cluster; otherwise port-forward.
### Engineering details worth getting right
- **Concurrency budget.** Don't unbounded-spawn. Use a Semaphore capped at e.g. 8 concurrent k8s API calls so you don't hammer the apiserver.
- **Timeout discipline.** Per-check timeout (--timeout), then a global wall-clock timeout that wraps everything (e.g. 30s). A check that times out reports as unknown, not fail — they're different signals.
- **No interactive prompts.** Ever. Kubeconfig auth that requires browser flow should fail fast with a clear error pointing at kubectl auth.
- **Error messages name the next command.** Every error message ends with what to run next. "Cannot reach Postgres at localhost:5432: connection refused. → kubectl get svc -n nico postgres"
- **Stable output ordering.** Layer order is fixed; findings within a layer sorted by severity then by ID. Diff-friendly across runs.
---
## nico-correlate
Given an ID, dump everything related to it from every source. One tool, one ID, one comprehensive view.
### Interface

nico-correlate <ID> [OPTIONS]
ARGS:
  <ID>                          Workflow ID, host ID, request ID, or tenant ID
OPTIONS:
  -t, --type <KIND>             ID type: workflow|host|request|tenant|auto
                                  [default: auto]
      --since <DURATION>        Look-back for logs/events [default: 1h]
      --until <DURATION>        Look-forward from start [default: now]
      --sources <LIST>          Comma-separated sources to include
                                  (temporal,logs,postgres,k8s,redfish,all)
                                  [default: all]
  -j, --json                    Output JSON
      --timeline                Print events as a sorted timeline (default in human)
      --tail                    Keep watching for new events after initial dump
      --pod <PATTERN>           Limit log search to matching pods
      --no-color                Disable color
  -h, --help                    Show help

Exit codes:
- 0 — found data for the ID
- 1 — ID format unrecognized or not found in any source
- 2 — partial: some sources unavailable (e.g. Postgres unreachable)
### ID type detection (--type auto)
Heuristics, in order:
1. wf-..., hp-..., host-prov-... prefixes → workflow
2. UUID → check Postgres for matching primary key in hosts, tenants, etc.
3. req-... → request ID; search logs only
4. Otherwise prompt user with --type hint and exit 1
Always print the detected type on the first line so the user can override if wrong.
### What you see (default — timeline view)

nico-correlate hp-7f3a2c                                  (workflow)
Workflow:    HostProvisioning hp-7f3a2c
Started:     2026-04-30 13:35:18Z (47m ago)
Status:      Running
Tenant:      tenant-acme  (acme.corp)
Host:        host-r12u5  (rack 12, slot 5)
Worker:      site-agent-7d9f4-x2vqp
Timeline (102 events, showing 24 most relevant):
  13:35:18  REST       POST /v1/hosts                          req-a83b
  13:35:18  REST       → start workflow HostProvisioning       wf hp-7f3a2c
  13:35:19  Temporal   WorkflowExecutionStarted
  13:35:19  Postgres   INSERT hosts (id=host-r12u5, state=Pending)
  13:35:20  Site Agent activity ValidateInput → completed
  13:35:21  Site Agent activity ReserveCapacity → completed
  13:35:22  Postgres   UPDATE hosts SET state='Reserved' WHERE id=host-r12u5
  13:35:24  Site Agent activity PowerOnBMC → started
  13:35:31  Redfish    POST .../Actions/ComputerSystem.Reset → 204
  13:36:02  Site Agent activity PowerOnBMC → completed (38s)
  13:36:03  Site Agent activity WaitForPxe → started
  ...
  14:09:40  Site Agent activity ConfigureDPU → started
  14:09:42  DPU        POST /api/network/segments → 200
  14:09:55  Site Agent activity ConfigureDPU → attempt 1 failed (Redfish 503)
  14:10:25  Site Agent activity ConfigureDPU → attempt 2 retrying (backoff 30s)
  14:11:25  Site Agent activity ConfigureDPU → attempt 2 failed (Redfish 503)
  14:13:25  Site Agent activity ConfigureDPU → attempt 3 retrying (backoff 120s)
  ...
  [stuck here, last update 12m ago]
Postgres state (current):
  [hosts.id](http://hosts.id)            host-r12u5
  hosts.state         Provisioning
  hosts.tenant_id     tenant-acme
  hosts.dpu_id        dpu-bf3-r12u5
  hosts.updated_at    2026-04-30 14:09:42Z (12m ago)
K8s pods touched:
  site-agent-7d9f4-x2vqp    Running   1 restart 18m ago
  core-6b8d9-q4nxz          Running   0 restarts
  scout-5f7c2-h8pmd         Running   0 restarts
Likely diagnosis:
  ConfigureDPU activity is in retry loop against Redfish (503 responses).
  → kubectl logs site-agent-7d9f4-x2vqp --since=15m | rg ConfigureDPU
  → curl -k https://mock-bmc/redfish/v1/  # is the mock BMC up?
  → temporal workflow show -w hp-7f3a2c   # full event history

This is the killer feature. Forty-five seconds of typing across four tools, replaced by one command that gives you a *narrative*. Note the "Likely diagnosis" section — keep it conservative (only fire on obvious patterns like "activity at max retries with consistent error code") and always end with commands the user runs themselves. The tool suggests; the human decides.
### What you see (--json)
json
{
  "version": 1,
  "id": "hp-7f3a2c",
  "id_type": "workflow",
  "entity": {
    "workflow": { "id": "hp-7f3a2c", "type": "HostProvisioning", "status": "Running", ... },
    "host": { "id": "host-r12u5", "state": "Provisioning", ... },
    "tenant": { "id": "tenant-acme", ... }
  },
  "events": [
    { "ts": "2026-04-30T13:35:18Z", "source": "rest", "kind": "http_request", "method": "POST", "path": "/v1/hosts", "request_id": "req-a83b" },
    { "ts": "2026-04-30T13:35:18Z", "source": "rest", "kind": "workflow_started", "workflow_id": "hp-7f3a2c" },
    ...
  ],
  "state": {
    "postgres": { "hosts": { ... } },
    "kubernetes": { "pods": [ ... ] }
  },
  "diagnosis": {
    "pattern": "activity_retry_exhaustion",
    "activity": "ConfigureDPU",
    "error_signature": "Redfish 503",
    "next_commands": [
      "kubectl logs site-agent-7d9f4-x2vqp --since=15m | rg ConfigureDPU",
      "..."
    ]
  }
}

### Source plugins
Each source is a module implementing a Source trait:
rust
#[async_trait]
trait Source {
    fn name(&self) -> &'static str;
    async fn collect(&self, id: &CorrelationId, opts: &Opts) -> Result<Vec<Event>>;
    async fn collect_state(&self, id: &CorrelationId, opts: &Opts) -> Result<StateSnapshot>;
}

Initial sources:
- **temporal**: workflow event history, mapped to events with consistent ts and kind
- **logs**: query Loki/Datadog if available, else stream pod logs and grep. Default to k8s log streaming for local dev
- **postgres**: SELECTs against tables related to the entity type (hosts, tenants, network_segments, audit_log)
- **k8s**: events touching the relevant pods, plus current pod state
- **redfish**: if a host ID is in scope, query the corresponding mock BMC for current power/boot state
- **rest**: REST access logs if available
Each source can fail independently and reports unavailable rather than crashing the whole tool. Final output annotates which sources contributed.
### Tail mode
--tail keeps the tool running, polling each source for new events and printing them as they arrive. Format identical to the timeline. This is for "watching a workflow currently in flight" — you start a workflow, run nico-correlate <id> --tail, and watch the system narrate itself in real time. Equivalent to having Temporal UI, four log tails, and a Postgres watch query open simultaneously.
Implementation: each source exposes an optional watch() stream. Merge with tokio_stream::StreamExt::merge.
### Engineering details worth getting right
- **Strict ID hygiene.** Never log the ID into your own tool's output in a way that can be confused with the system's events. Prefix with [nico-correlate] or similar.
- **Time alignment.** All sources normalize timestamps to UTC, format consistently, and ideally use the same monotonic ordering when timestamps tie. Subsecond precision matters for fast operations.
- **Bounded memory.** Don't read all logs at once. Stream, filter on the fly, drop non-matching lines. A misuse against a busy production cluster shouldn't OOM your laptop.
- **Source extension is the design.** Adding ClickHouse logs, Datadog APM, or a future MCP-style source should be a new file in sources/ and a registry entry. No core changes.
---
## Build order
Day 1 — nico-common skeleton: config loading, output module, k8s client wrapper, error types. Test against your kind cluster.
Day 2 — nico-doctor cluster + logs + health layers. Get the human output looking right before adding more layers; output quality matters more than feature count.
Day 3 — nico-doctor workflows + grpc + postgres layers. JSON output. Exit codes. Wire into a pre-deploy check script.
Day 4-5 — nico-correlate with temporal + postgres sources. Timeline output. ID auto-detection.
Day 6 — nico-correlate logs + k8s sources. Diagnosis hints (only the conservative ones).
Day 7 — --tail mode and polish. Tests. README with screenshots.
You'll know it's done when you find yourself reaching for these two tools as the first move in incidents instead of the underlying ones, *and* you still drop into the underlying tools the moment a question gets specific. That's the right division of labor.
---
## What not to add
- **No web UI, no TUI.** Both tools are stdin-stdout, scriptable, and disposable.
- **No persistent state.** No local cache, no SQLite, no daemons. Each invocation is independent.
- **No remediation actions.** Read-only. The moment you add "fix it" buttons, you've created a tool people use without understanding, and you've taken on a security and audit burden you don't want.
- **No alerting.** Datadog already does that. These tools are for active investigation, not passive monitoring.
- **No self-update or telemetry.** A binary you trust enough to run in a sensitive environment shouldn't phone home.
The discipline is: small surface, high quality output, plays well with shell.
