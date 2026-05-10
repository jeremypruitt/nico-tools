# NICo Networking — Master Learning Plan

> **Forcing function:** In a few weeks I need to be the person at IREN who can (a) explain HBN to a customer, (b) debug a stuck PXE boot, and (c) read a UFM congestion report. Until I can do all three, this doc drives my days.

-----

## What this doc is

This is the orchestrator for a focused, time-boxed effort to become IREN’s in-house expert on NICo networking, anchored on a 100x GB300 install 4–6 weeks out.

It is a syllabus, a manifest, and a workflow guide in one file. It does **not** contain the learning itself — that lives in topic files (`docs/learning/topics/<NN>-<slug>.md`) produced by focused Claude Code sessions, one topic at a time, each with its own clean context window.

**Why one-topic-per-conversation.** A single long Claude conversation gets slow, forgetful, and muddled. Splitting by topic keeps each thread sharp, lets me run topics in parallel when I want, and produces durable artifacts I can re-read. This master doc is what every new conversation reads first to know where it fits.

**Why this lives in `nico-tools`.** The CLI I’m building is the eventual home for the operational muscle memory I develop here. Kata that teach me something worth codifying become `nico doctor` checks, `nico correlate` rules, or `nico ops` panels. Keeping the learning plan and the tool in the same repo means insights don’t get stranded.

-----

## How to use this doc with Claude Code

**Starting a topic.** Open Claude Code in this repo and say:

> Read `docs/learning/MASTER.md`, then start topic `01-hbn`. Use the prompt template in the Workflow section. The NICo repos are at `../infra-controller-core` and `../infra-controller-rest` (clone them as siblings if not present).

Claude Code reads the master, reads the topic’s scope from the manifest, clones/reads the NICo repos, and produces `docs/learning/topics/01-hbn.md` plus any kata, diagrams, or CLI feature stubs.

**Resuming a topic.** Same as starting, but Claude Code reads the existing topic file first and continues where it left off. The topic file’s “Status & next” section tells it what’s done and what’s next.

**Running topics in parallel.** Open multiple Claude Code sessions, each pinned to a different topic. They don’t talk to each other — coordination happens through commits to the manifest table below and through the topic files themselves. Two topics that depend on each other should be done sequentially, not in parallel; the manifest’s “Prereqs” column flags this.

**Daily kata.** Kata live in `docs/learning/kata/` and are run against a local devspace deployment of NICo (week 1–2) or the NVIDIA lab (week 3+). Kata don’t have their own Claude conversations — they’re short enough to do in whatever conversation is active, or solo. New kata get added to `kata/INDEX.md` as I encounter things worth practicing.

**Updating the manifest.** When a topic moves status, edit the table below and commit. Master doc is the source of truth for “what’s done.”


-----

## Workflow: prompt template for starting a topic

Paste this when starting any topic conversation. Fill in the bracketed bits.

```
I'm working through the NICo networking learning plan in this repo (docs/learning/MASTER.md).
Read that first.

Topic for this conversation: [NN-slug, e.g. 01-hbn]
Topic file (create if absent): docs/learning/topics/[NN-slug].md

Sources of truth:
- ../infra-controller-core (NVIDIA/infra-controller-core, Rust + gRPC)
- ../infra-controller-rest (NVIDIA/infra-controller-rest, REST/Go)
- NICo docs: ../infra-controller-core/docs/ (architecture, components, dpu-operations, ...)
- This repo's CONTEXT.md, docs/prds/, docs/adrs/ — for the doctor/correlate side of the world (already-shipped vocabulary and decisions)
- NVIDIA NCP Software Reference Guide (web, when needed)

My background: 17 years at Juniper in roles other than network engineering;
strong on cloud, K8s, IaC, gRPC, Postgres, Rust-reading; weak on InfiniBand,
DPUs, NCCL, optical/cabling. Currently L1-2 on most networking topics
unless flagged otherwise in the topic file.

What I need from this session:
1. Conceptual model — what is this thing, why does it exist, what does it touch
2. NICo-specific implementation — where in the code, which API objects, which services
3. Failure modes — what goes wrong, what the symptoms look like, where to look
4. Two or three kata I can run today (in devspace) or note for the lab
5. CLI hooks — anything that should become a `nico doctor / correlate / ops` feature
6. Open questions to bring back to my boss or to NVIDIA

Output goes in the topic file. Use plain markdown, with diagrams as ASCII or
mermaid where useful. Don't pad. I'd rather have one tight page I'll re-read
than five I won't.

When you're done, update:
- The topic file's "Status & next" section
- The manifest table in MASTER.md (status column for this topic)
- kata/INDEX.md if you added kata
- cli-features.md if you proposed CLI features
```

-----

## Topic manifest

Order matters for the first six rows. After that, parallelize as I please.

|# |Topic                                                          |Status     |Prereqs     |Est. time|Why this slot                                                                           |
|--|---------------------------------------------------------------|-----------|------------|---------|----------------------------------------------------------------------------------------|
|01|HBN (Host-Based Networking) on BlueField                       |not started|—           |1–2 days |Conceptual center of NICo. Everything else is downstream.                               |
|02|NICo control plane shape                                       |not started|01          |0.5 day  |Map of the components so every later topic knows where it lives.                        |
|03|Bare-metal lifecycle end-to-end                                |not started|01, 02      |1 day    |The spine of every NICo design conversation.                                            |
|04|DPU lifecycle & zero-trust model                               |not started|01, 02      |1 day    |Deepens 03 around the DPU specifically.                                                 |
|05|InfiniBand fundamentals + UFM                                  |not started|—           |3–4 days |IB-first install. Largest gap to my background. Can start in parallel with 01 if needed.|
|06|NVLink domain & NMX on GB300 NVL72                             |not started|05 (helpful)|1–2 days |GB300-specific fabric. Can’t ignore.                                                    |
|07|Site networking: DHCP, PXE, DNS (carbide-dns, unbound)         |not started|02          |1 day    |Boot path debugging — directly serves “debug a stuck PXE boot.”                         |
|08|MetalLB + BGP to TOR, site VIPs                                |not started|02          |0.5 day  |Familiar territory, but the NICo-specific wiring matters.                               |
|09|Site Agent & Temporal workflows                                |not started|02          |1 day    |Cross-site orchestration; understand before things break in prod.                       |
|10|Vault, certs, CSRs in the NICo trust model                     |not started|02, 04      |0.5 day  |Touches networking via mTLS between core, REST, agents, DPUs.                           |
|11|Ethernet path: Spectrum-X, Netris, DOCA SDN                    |not started|01, 05      |1–2 days |Roadmap, not day-one IB install — but expected to come.                                 |
|12|k0rdent ↔ NICo integration (incl. k0rdent AI)                  |not started|02          |1 day    |Mirantis side of the stack. Confirm what’s real vs aspirational.                        |
|13|Observability: Prometheus, Grafana, logcli, ssh-console        |not started|02          |0.5 day  |Where I’ll actually look when something is wrong.                                       |
|14|Multi-tenant networking model: VPCs, isolation, shared services|not started|01, 04      |1 day    |The “why” behind HBN, in customer language.                                             |

**Status values:** `not started` → `in progress` → `draft complete` → `reviewed` → `kata seeded` → `done`. A topic is `done` when I can explain it on a whiteboard cold and at least two kata for it exist in `kata/INDEX.md`.

-----

## Topic scopes (one paragraph each)

These are intentionally short. Detail goes in the topic files.

**01 — HBN.** Host-Based Networking runs on the BlueField DPU and terminates VXLAN/EVPN overlays at the compute edge instead of at the leaf switch. Goal: understand why this architecture exists (multi-tenant isolation without per-tenant switch config), how NICo programs HBN on the DPU, what the data path looks like for a tenant packet from VM/container → DPU → underlay → DPU → VM/container, and the relationship between DOCA, HBN, and the SDN controller. Outcome: I can draw the HBN data path on a whiteboard and explain why the leaf switch stays “dumb.”

**02 — Control plane shape.** Map of carbide-core (Rust gRPC), carbide-rest (REST/OpenAPI), site-agent (Temporal northbound), carbide-api, carbide-dns, scout, dpu-agent, Postgres (forgedb), Vault. For each: what it does, who calls it, where its config lives, where its logs go. Outcome: when someone says “the site agent isn’t reconciling,” I know which box, which logs, which API.

**03 — Lifecycle end-to-end.** Rack server → scout discovers DPU → NICo configures BMC + DPU BMC → PXE boot of host → firmware validation → DPU agent online → host allocated to tenant → HBN config pushed → tenant boots → tenant releases → DPU wipes → back to pool. Outcome: I can narrate the full flow and point at which NICo component drives each transition.

**04 — DPU lifecycle & zero trust.** BlueField is owned by NICo, not the host. Host can’t reconfigure the DPU. NICo manages DPU firmware, DPU OS image, DPU OOB network. Cover: BMC vs DPU BMC vs DPU OOB, the three networks, the trust boundary, what changes between tenants. Outcome: I can explain to a security-minded customer why a compromised tenant host can’t escape the DPU boundary.

**05 — InfiniBand + UFM.** IB fundamentals: subnet manager (OpenSM), LIDs, GUIDs, PKeys, partitions, SHARP, congestion control. UFM Enterprise: GUI, telemetry, fabric validation, partition management. Outcome: I can read a UFM dashboard, identify a congested link, and explain partition-based tenant isolation. This is the largest topic — expect to revisit.

**06 — NVLink domain & NMX.** GB300 NVL72 has 72 GPUs in a rack-scale NVLink domain managed by NMX. This is a third fabric (alongside IB and Ethernet), internal to the rack, much faster. Cover: what NMX does, how NICo interacts with it (or doesn’t), failure modes specific to NVLink at rack scale. Outcome: I know when “the network” means NVLink vs IB vs Ethernet.

**07 — Site networking: DHCP, PXE, DNS.** NICo runs DHCP for BMC/DPU-BMC/DPU-OOB underlays + host overlay, PXE for host boot, carbide-dns for site queries, unbound for recursive. Cover: config sources, scopes, lease lifecycle, PXE chain, DNS zones. Outcome: I can debug a host that won’t PXE — by knowing exactly which logs to read in which pod in what order.

**08 — MetalLB + BGP to TOR.** Site VIPs, IP pool config, BGP peering to TOR switches, how the carbide-rest API gets a stable address. Outcome: I can read MetalLB config and BGP status and tell whether the site is reachable from the cloud control plane.

**09 — Site Agent & Temporal.** Site Agent maintains a northbound Temporal connection to NICo REST. REST can live in cloud or on-site. Multiple cores can connect to one REST. Cover: Temporal workflow basics, retry/timeout semantics, how to inspect stuck workflows. Outcome: when a site goes silent, I know whether it’s network, agent, Temporal, or REST, and where to start.

**10 — Vault, certs, CSRs.** Vault issues certs for the trust mesh between core/REST/agents/DPUs. Cover: what’s signed, rotation, what breaks when a cert expires. Outcome: I can diagnose a “suddenly nothing reconciles” failure that turns out to be cert expiry.

**11 — Ethernet path: Spectrum-X, Netris, DOCA SDN.** When IREN moves beyond IB-first, this is the alternative fabric. Netris is the named partner for switch-side configuration (per NICo issue #938). DOCA is the SDK on BlueField. Outcome: I can speak to the Ethernet roadmap without bluffing.

**12 — k0rdent ↔ NICo.** k0rdent is Mirantis’s K8s-based cluster management platform. k0rdent AI is the AI-specific build. Confirm whether a NICo provider/connector exists today, what it does, what’s aspirational. Outcome: I know exactly where the handoff is between k0rdent (cluster lifecycle) and NICo (bare-metal + DPU + HBN).

**13 — Observability.** Prometheus `/metrics` on port 9009 for hardware health, Grafana for dashboards, logcli for logs, ssh-console for serial. Cover: where each lives in the deployment, what queries to keep in muscle memory. Outcome: I have a tab order for incident response.

**14 — Multi-tenant networking.** Pull HBN, partitions, VPC-on-DPU, shared services network, route leaks together into a customer-facing story. Outcome: I can do a 20-minute whiteboard for a prospect.

-----

## Kata track

Kata are short hands-on exercises, run daily, that build muscle memory alongside conceptual learning. They live in `docs/learning/kata/` with one file per kata and an `INDEX.md` that tracks them all.

**Cadence.** Aim for 30–60 minutes of kata per day, separate from topic deep-dives. Kata that take longer get split.

**Targets by week:**

- Week 1: devspace local mocks. Make the system, break it intentionally, fix it.
- Week 2: devspace + first reads of real NICo logs/state.
- Week 3: NVIDIA lab. Real DPUs, real switches, real lifecycle transitions.
- Week 4+: GB300 hardware at IREN.

**Seed kata for week 1** (each gets its own file when I run it):

- `k01-devspace-bringup.md` — stand up NICo locally per the dev guide; document every step that wasn’t obvious.
- `k02-grpc-tour.md` — use grpcurl with reflection to enumerate the API surface; pick five RPCs and call them.
- `k03-kill-the-dhcp-pod.md` — kill the DHCP pod mid-PXE boot of a mock host; observe failure mode end-to-end; restore.
- `k04-trace-a-tenant-allocation.md` — trace a tenant allocation request from REST → core → site agent → DPU agent in logs.
- `k05-postgres-forgedb-tour.md` — connect to forgedb, list tables, find the row representing a managed host, identify the FK graph.
- `k06-cert-expiry-sim.md` — force a cert near-expiry; observe what NICo does; confirm rotation path.
- `k07-temporal-workflow-inspect.md` — open Temporal UI, find a running workflow, identify a stuck activity.

Kata are the connective tissue between knowing and operating. They are also the primary feedstock for the CLI.

-----

## CLI integration: where learning becomes tooling

The `nico` CLI in this repo has three relevant subcommands: `doctor` (health checks), `correlate` (pattern matching across logs/metrics/state), and `ops` (operator dashboard). The learning plan should produce a backlog of features for each, captured in `docs/learning/cli-features.md` (created lazily — no need to seed it empty).

**Rule of thumb for what becomes a CLI feature.** If during a kata or topic I find myself running the same multi-step diagnostic more than once, it becomes a `doctor` check. If I find myself joining two data sources mentally (e.g., “this DPU’s BMC IP from forgedb plus the corresponding lease from DHCP logs”), it becomes a `correlate` rule. If I find myself eyeballing a sequence of states in order to know whether things are healthy, it’s an `ops` panel.

**What this doc does NOT do.** It does not design the CLI. The CLI’s architecture is its own track and should not be constrained by what the learning plan happens to surface first. Features get listed as candidates, not commitments.

**Initial CLI candidates** (to grow as topics complete):

- `nico doctor pxe` — walk the PXE chain for a given host MAC and report exactly where it broke.
- `nico doctor certs` — list every cert in the trust mesh with days-to-expiry.
- `nico correlate host-allocation <id>` — pull the full lifecycle of a host allocation across REST, core, site-agent, dpu-agent, and Temporal logs into one timeline.
- `nico ops fabric` — at-a-glance IB fabric health pulled from UFM (once UFM access is available).
- `nico ops dpus` — DPU inventory with firmware version, last-checkin, current tenant, HBN status.

These are sketches. Real specs land in `cli-features.md` as topics finish.

-----

## Glossary

Defined here so every topic file can assume them.

**BMC (Baseboard Management Controller).** Out-of-band controller on a server; lets NICo power-cycle, reimage, and inspect a host without the host’s OS.

**BlueField (BF2/BF3/BF4).** NVIDIA’s DPU. A NIC with its own ARM CPU, memory, and Linux. Owned by NICo, not by the host.

**Carbide.** Internal codename for NICo. Image names (`carbide-core`, `carbide-rest`, `carbide-api`, `carbide-dns`) and Helm charts use Carbide. Public docs and the GitHub repo use NICo. Same thing.

**DOCA.** NVIDIA’s SDK and runtime for BlueField. HBN runs on top of DOCA.

**DPU (Data Processing Unit).** Generic term; in this stack it always means BlueField.

**EVPN.** Ethernet VPN — control-plane protocol (BGP-based) for VXLAN overlays. Used by HBN.

**forgedb.** The Postgres database NICo uses for state.

**HBN (Host-Based Networking).** Software on the BlueField DPU that terminates VXLAN/EVPN overlays at the compute edge. The single most important networking concept in NICo.

**InfiniBand (IB).** Lossless, low-latency fabric used for GPU-to-GPU traffic in AI clusters. Requires a centralized subnet manager. Quantum/Quantum-2/Quantum-X800 are NVIDIA’s IB switch lines.

**k0rdent / k0rdent AI.** Mirantis’s K8s-based cluster management platform. AI is the AI-workload-tuned variant. Mirantis is now part of IREN.

**LID.** Local IDentifier — IB’s link-layer address. Assigned by the subnet manager.

**MetalLB.** Kubernetes service-type-LoadBalancer implementation for bare-metal clusters; here used with BGP to TOR.

**Netris.** Third-party SDN controller for Spectrum-X (Ethernet) fabrics. Named partner in NICo’s roadmap for switch-side config.

**NICo.** NCX Infra Controller. The thing this whole doc is about.

**NMX.** NVLink Management eXtension. Configures the rack-scale NVLink domain on GB300 NVL72.

**NVLink domain.** The 72-GPU NVLink fabric inside a GB300 NVL72 rack. Third fabric, alongside IB and Ethernet.

**OpenSM.** Open-source InfiniBand subnet manager. Runs under UFM in managed deployments.

**Partition / PKey.** IB’s tenant isolation primitive. Like a VLAN but for IB.

**PXE.** Pre-boot eXecution Environment. Network boot.

**REST (the layer).** carbide-rest — the OpenAPI-fronted layer that sits in front of carbide-core. Can live cloud-side or site-side.

**scout.** NICo service that discovers DPUs at initial deployment.

**Site Agent (Elektra).** Maintains the northbound Temporal connection from a site to NICo REST.

**Spectrum / Spectrum-X.** NVIDIA’s Ethernet switch line. Spectrum-X is the AI-optimized variant with RoCE-friendly congestion control.

**TOR.** Top-of-rack switch.

**UFM (Unified Fabric Manager).** NVIDIA’s IB fabric manager. Tiers: Telemetry, Enterprise, Cyber-AI.

**VXLAN.** Layer-2-over-layer-3 overlay. Encapsulation used by HBN.

-----

## Failure modes I want to avoid in this plan itself

- **Plan as procrastination.** This doc is not the work. The topic files are the work. If I’m editing the master more than I’m filling out topics, something’s wrong.
- **Ordering rigidity.** If my boss asks about Netris on day three, I do Netris on day three. The order is a default, not a contract.
- **Pretending kata are tested.** The seed kata are guesses based on reading the repo. If a kata turns out to be wrong or pointless, I delete it and note why in `kata/INDEX.md`.
- **CLI feature creep.** The CLI is its own project with its own tempo. Features listed here are candidates. Don’t let the learning plan dictate the CLI roadmap.
- **Missing the install.** All of this is in service of the GB300 install in 4–6 weeks. If a topic isn’t moving me toward being useful on that install, it can wait.

-----

## Status log

Append-only. One line per work session.

- `[YYYY-MM-DD]` Master doc created. Next: stand up devspace and start topic 01-hbn.
