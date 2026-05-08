# 01 — HBN (Host-Based Networking) on BlueField

> **Status:** draft complete (2026-05-07). Open questions at the bottom.
> **Goal restatement:** I can draw the HBN data path on a whiteboard and explain why the leaf switch stays "dumb."

## Sources used

- `infra-controller-core/crates/agent/src/hbn.rs` — the HBN module on the DPU agent.
- `infra-controller-core/crates/agent/src/{lib.rs,command_line.rs,health.rs,ethernet_virtualization.rs}` — agent main loop, CLI flags, health probes, quarantine.
- `infra-controller-core/crates/rpc/proto/forge.proto` — the gRPC the DPU agent calls (`GetManagedHostNetworkConfig`, `RecordDpuNetworkStatus`).
- `infra-controller-rest/rla/internal/nicoapi/nicoproto/nico.proto` — REST-side mirror of those RPCs + `TriggerDpuReprovisioning`.
- `infra-controller-core/docs/architecture/dpu_configuration.md` — the canonical declarative-config doc + JSON example.
- `infra-controller-core/docs/dpu-operations.md` — operator-side BMC/BFB/OVS commands.
- `infra-controller-core/docs/glossary.md` — VXLAN/VNI/VTEP/EVPN definitions in NICo's words.
- `infra-controller-core/crates/secrets/src/credentials.rs` — `CredentialKey::DpuHbn` Vault path.
- `infra-controller-core/crates/admin-cli/src/dpu/versions/cmd.rs` — operator-facing version table.
- `infra-controller-rest/site-workflow/pkg/activity/machine.go` — Temporal activity that fetches DPU network config.

`infra-controller-core/book/src/` exists in MASTER.md but is empty in this checkout. Re-check before next topic.

---

## 1. Conceptual model

**HBN is the DPU acting as the tenant's leaf switch.** It's a containerized NVUE-managed networking stack (`doca-hbn` container, running on each BlueField) that:

- terminates the **VXLAN/EVPN overlay** at the compute edge (DPU = VTEP),
- runs **FRR with BGP/EVPN** northbound to a route-server / TOR (RFC 5549 unnumbered, BGP EVPN AF),
- enforces tenant isolation via per-tenant **VNIs** (24-bit) — VLANs (12-bit) only used DPU↔host,
- enforces quarantine via NVUE-programmed **ACLs** (`BlockAllTraffic`) when the DPU isn't known to the control plane.

**Why this exists** (the customer story): the leaf switch stops being a tenant-aware policy enforcement point. It just routes underlay. All overlay state — VRFs, route maps, ACLs, BGP peering, MAC learning — lives on the DPU, owned by the cloud operator's control plane (NICo), not the host or the network team. Adding a tenant doesn't touch a switch.

**Why this matters operationally:** a leaf swap is a hardware swap, not a config event. A new tenant is a config push to N DPUs, not a switch change-window. Multi-tenancy scales with VNI space (16M), not VLAN space (4K).

**Juniper Comparison:** think of HBN as the EVPN-VXLAN PE function pushed onto the host's NIC instead of QFX/MX, with NVUE as the (proprietary) JUNOS analogue and FRR underneath. The route server is your route reflector. The TOR is now MPLS-style P-only.

```
                        ┌───────────────────────┐
                        │  carbide-core (Rust)  │── Postgres (forgedb)
                        │     gRPC :8443        │
                        └───────────┬───────────┘
                                    │  GetManagedHostNetworkConfig (poll ~30s)
                                    │  RecordDpuNetworkStatus
                ┌───────────────────┼───────────────────┐
                ▼                   ▼                   ▼
         ┌────────────┐      ┌────────────┐      ┌────────────┐
         │ DPU agent  │      │ DPU agent  │      │ DPU agent  │  (one per BF on each compute host)
         │  (mTLS)    │      │            │      │            │
         │  applies → │      │            │      │            │
         │  doca-hbn  │      │  doca-hbn  │      │  doca-hbn  │  container; NVUE config
         │  + FRR     │      │  + FRR     │      │  + FRR     │
         └─────┬──────┘      └─────┬──────┘      └─────┬──────┘
               │  underlay BGP-EVPN to route server / TOR
               ▼                   ▼                   ▼
                ─── plain L3 underlay (TOR is dumb) ───
```

### Data path for one tenant packet

1. VM/container on host emits frame on a VLAN (DPU↔host VLAN, e.g. 14/16 in the example config).
2. DPU's HBN container takes the frame off the host-facing PF/SF, looks up the VNI for that VLAN/VRF.
3. HBN encaps in VXLAN with `vpc_vni` (or admin VNI), sources from `loopback_ip`.
4. UDP/IP underlay hop to remote DPU's loopback (BGP-EVPN learned route, RT-2/RT-5).
5. Remote DPU decaps, looks up dest by inner MAC/IP, hands frame to its host on the matching VLAN.

The TOR/leaf in the middle never sees the inner packet. It sees DPU-loopback to DPU-loopback IP unicast. That's the whole point.

---

## 2. NICo-specific implementation

### 2a. Control plane shape

**It's a pull model.** The DPU agent (Rust binary on the BF) polls core; core does not push. This is in `infra-controller-core/crates/rpc/proto/forge.proto`:

```proto
rpc GetManagedHostNetworkConfig(ManagedHostNetworkConfigRequest)
    returns (ManagedHostNetworkConfigResponse);
rpc RecordDpuNetworkStatus(DpuNetworkStatus) returns (google.protobuf.Empty);
```

The response is **declarative** and **versioned twice**:
- `managed_host_config_version` — bumps on per-host wiring changes (loopback, ASN, BGP peers, VLANs).
- `instance_network_config_version` — bumps on per-tenant lifecycle events (new VPC, quarantine, release).

The DPU agent reports back which version it has actually applied. Core uses that to drive the API state machine ("Provisioning" → "Running" → "Terminating"). If the agent returns a stale version, the workflow waits — there is no out-of-band confirmation channel.

`NotFound` from `GetManagedHostNetworkConfig` is treated as **isolation** — the DPU goes to a no-network state. Important: this is the trust mechanism. An unknown DPU has zero connectivity, not a default config.

### 2b. The HBN module on the DPU agent

`infra-controller-core/crates/agent/src/hbn.rs` is the surface that talks to the DPU's local HBN container. Two config modes, selected by `HBN_CONFIG_MODE` env var (`HbnConfigMode` enum in `command_line.rs`):

- **`container-exec`** — legacy. Runs commands inside the `doca-hbn` container via `crictl exec`. Has to deal with mgmt VRF isolation (`IGNORE_MGMT_VRF` toggle).
- **`nvue-rest`** — modern. Talks to NVUE's REST API on localhost.

Version constraints:
- `NVUE_MINIMUM_HBN_VERSION = "2.0.0-doca2.5.0"` — anything older, the agent won't configure it.
- `FMDS_MINIMUM_HBN_VERSION = "1.5.0-doca2.2.0"` — flow management data store gate.

The agent also writes two "always-on" pieces of HBN-container config (`HBNContainerFileConfigs::ensure_configs`) for ARP accept policy and neighmgr subnet checks — these are deliberately not part of the declarative config and are set once at agent startup.

### 2c. Where the config lives

Declarative JSON (example in `docs/architecture/dpu_configuration.md`) carries all of:

| Field | What it is |
|---|---|
| `loopback_ip` | DPU's BGP source IP (the VTEP IP) |
| `asn` | DPU's BGP ASN |
| `route_servers` | BGP peers (RR equivalents) |
| `dhcp_servers` | underlay DHCP relay targets |
| `vni_device` | `vxlan48` (single shared VXLAN dev) or `""` (admin net) |
| `vpc_vni` | tenant overlay VNI |
| `admin_interface`, `tenant_interfaces` | host-facing VLAN + VNI + IPs |
| `vpc_prefixes`, `vpc_peer_prefixes`, `vpc_peer_vnis` | VRF / inter-VPC route leak |
| `network_virtualization_type` | enum: `ETV`, `FNN`, … |

**Storage:** Postgres / forgedb. Migrations confirm: `20241128073706_dpu_vpc_loopback.sql`, `20230830063454_dpu_reprovisioning.sql`. Per-DPU credentials (HBN-specific) live in Vault at `machines/{machine_id}/dpu-hbn` (`CredentialKey::DpuHbn`).

### 2d. Trust boundary

mTLS, end to end. `agent/src/lib.rs` loads `forge_system.client_cert/key` and passes them into the gRPC client. The agent reports its own `client_certificate_expiry_unix_epoch_secs` in `DpuNetworkStatus` — so the control plane has a feedback loop on its own cert plumbing breaking, before it does break.

The host can't reach this — host doesn't have the cert, doesn't know the gRPC endpoint, doesn't have a path to write into the doca-hbn container. The DPU agent is the only thing that mutates HBN state.

### 2e. Quarantine

`agent/src/ethernet_virtualization.rs` implements `build_quarantined_network_security_group_rules()`. When the response carries `quarantine_state.is_some()`, the agent applies `BlockAllTraffic` rules via NVUE — concretely, deny-all ACLs on the data path. Tests: `test_with_tenant_nvue_quarantined`, `test_with_tenant_fnn_quarantined`. This is the *enforcement* of the trust boundary; the *gating* is the version-mismatch / NotFound logic above.

---

## 3. Failure modes (the symptoms-and-where-to-look list)

| Symptom | Likely cause | Where to look |
|---|---|---|
| Tenant can't ping peer in same VPC | `vpc_vni` mismatch, BGP-EVPN type-2 route missing, or DPU's BGP session down to route server | `RecordDpuNetworkStatus.dpu_health` alerts; FRR `vtysh -c 'show bgp l2vpn evpn summary'` inside HBN container |
| Tenant in "Provisioning" forever | DPU agent reports stale `instance_network_config_version` | `RecordDpuNetworkStatus` rows in DB; `nico admin-cli dpu versions` |
| All DPU traffic dropped right after a config push | `PostConfigCheckWait` health probe stuck (30s grace), or quarantine accidentally applied | `agent/src/health.rs` probes; check `quarantine_state` in last response |
| DPU isolated (BlockAllTraffic) but should be live | `GetManagedHostNetworkConfig` returns `NotFound` — DPU not registered or scout discovery hasn't completed | core logs for the machine_id; `MachineQuarantineState` in DB |
| Random tenant outage minutes after a routine change | mTLS cert near expiry — agent stops polling | `client_certificate_expiry_unix_epoch_secs` in last status; cert-issuer (Vault PKI) logs |
| HBN container won't start | HBN version below `NVUE_MINIMUM_HBN_VERSION` (2.0.0-doca2.5.0) | `crictl ps` on DPU; agent log line "HBN version too old" |
| Config push silently no-ops | Wrong `HBN_CONFIG_MODE` for the installed HBN — e.g. `nvue-rest` against pre-2.0 HBN | DPU agent env; agent startup log |
| `crictl exec` hangs | mgmt VRF isolation interfering | toggle `IGNORE_MGMT_VRF`, check 45s timeout in `run_in_container` |
| Logs missing | HBN logs at `/var/log/doca/hbn/` on the DPU, not on the host | `ssh-console` to DPU OOB, then tail in container |

**The tell I want in muscle memory:** if the API state and the DPU's reported state don't match, look at *both* version numbers (managed_host vs instance) before anything else. Most weird stuck states are version-drift, not network failure.

---

## 4. Kata (devspace, week 1)

Three I can run without lab hardware. Each gets its own file in `kata/` when I run it; seeds added to `kata/INDEX.md`.

- **k08-grpc-fetch-dpu-config** — `grpcurl` against carbide-core for `GetManagedHostNetworkConfig` with a fake-but-registered machine_id. Goal: read a real `ManagedHostNetworkConfigResponse` and identify every field in §2c by hand.
- **k09-simulate-dpu-version-drift** — point a mock dpu-agent at devspace core, send `RecordDpuNetworkStatus` with a *stale* `instance_network_config_version`, then watch the API state machine refuse to advance. Goal: feel the pull-and-version mechanic from the core's perspective.
- **k10-quarantine-flip** — flip a host's `MachineQuarantineState` in forgedb directly, watch the next `GetManagedHostNetworkConfig` response carry `quarantine_state` set, mock-apply the `BlockAllTraffic` ACLs on a dummy NVUE. Goal: confirm the trust enforcement path end-to-end.

Stretch (lab, week 3+):
- **k11-real-vxlan-trace** — on a real DPU pair, `tcpdump` the underlay between two loopbacks while pinging across the overlay. Identify VXLAN headers, VNI, EVPN MAC routes.

---

## 5. CLI hooks (candidates for `cli-features.md`)

These all came directly out of the failure-mode table. Captured as candidates, not commitments.

- **`nico doctor hbn <dpu-id>`** — single-DPU health check: HBN container running, version ≥ minimums, last-applied config version vs desired, BGP peer up, quarantine state, last status timestamp. One command, one verdict.
- **`nico doctor dpu-cert <dpu-id>`** — days-to-expiry on the dpu-agent client cert, pulled from the last `DpuNetworkStatus`. Saves an SSH hop. Rolls up across all DPUs in `nico doctor certs`.
- **`nico correlate hbn-config-drift <machine-id>`** — joins desired config (from forgedb) with last reported status (also forgedb), computes per-version drift age, and shows the relevant slice of `agent/src/health.rs` probe history. Exists because I'll mentally do this every time something is "stuck."
- **`nico ops hbn`** — table view: machine_id, HBN ver, NVUE ver, applied managed_host_ver, applied instance_ver, drift (s), quarantine, cert days. Sortable. The single panel I'd want during a tenant-onboarding incident.
- **`nico doctor dpu-isolation <machine-id>`** — answers "should this DPU be isolated?" by checking machine registration, scout discovery state, and quarantine state separately. Distinguishes "not yet known" from "deliberately quarantined" from "lost connection."

---

## 6. Open questions

For my boss / NVIDIA / next reading session.

1. **Route server topology.** `route_servers` in the DPU config is a list of IPs. Are these provisioned by NICo (a NICo component running BGP), or are they the customer's external RRs (Cumulus/SONiC route reflectors)? Day-1 ops question.
2. **HBN ↔ OVS.** `dpu-operations.md` references `ovs-vsctl` for inspection. Is OVS the host-facing dataplane that HBN/FRR feeds, or is it a parallel/legacy path? Want a concrete picture of the DPU's internal forwarding pipeline (probably: PF/SF → OVS bridge → HBN → VXLAN out).
3. **Multi-VPC on one DPU.** The proto carries one `vpc_vni`, plus `vpc_peer_vnis` for leaks. Does a single DPU host one tenant at a time, or many? If one — what triggers the wipe? `TriggerDpuReprovisioning` exists; need to read it.
4. **GB300 NVL72.** 72 GPUs in one rack on NVLink. Where are the BlueFields physically — one per compute tray (so 18 BFs/rack)? Does HBN run on each, or is there a NMX/NVLink-internal path that bypasses HBN for intra-rack? Topic 06 will resolve this but it affects how I think about HBN scope.
5. **Underlay BGP on the leaf.** Who configures the TOR's BGP-EVPN peering? NICo? Netris (topic 11)? Network team manually? Affects who I escalate a missing peer to.
6. **NVUE vs container-exec rollout.** What's the timeline / current default? In a real GB300 install, do I expect `nvue-rest`?
7. **Health probe catalog.** `health.rs` mentions `PostConfigCheckWait` — what other probe IDs exist, and which produce alerts that surface to the customer (`tenant_message`)?

---

## Status & next

- **Status:** draft complete; concept + control plane mapped, failure-mode table grounded in code, kata seeded, CLI candidates listed.
- **Promote to `reviewed`** after I run k08 (read a real response) and k10 (confirm quarantine path).
- **Promote to `done`** after I can whiteboard §1 cold and have run k11 in the lab.
- **Next topic:** `02-control-plane-shape` (this topic gestured at carbide-core, dpu-agent, scout, forgedb — need the full map).
