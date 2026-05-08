# Kata Index

Short hands-on exercises. Targets:
- Week 1–2: devspace local mocks (kill things, fix them)
- Week 3+: lab (real DPUs, real switches)
- Week 4+: GB300 hardware

Format: each kata gets its own file (`kNN-<slug>.md`) when first run. Until then it's a planned seed.

## Status legend

`seeded` → `run` → `repeatable` (I've done it twice and it's worth keeping). Drop kata that turn out wrong; record why here.

## Kata

| ID | Topic | Target | Status | Notes |
|----|-------|--------|--------|-------|
| k01 | devspace bringup — stand up NICo locally per dev guide | devspace | seeded | from MASTER seed |
| k02 | gRPC tour — grpcurl reflection, call 5 RPCs | devspace | seeded | from MASTER seed |
| k03 | kill the DHCP pod mid-PXE | devspace | seeded | from MASTER seed |
| k04 | trace tenant allocation across REST/core/site-agent/dpu-agent logs | devspace | seeded | from MASTER seed |
| k05 | forgedb tour — list tables, find a host row, walk FK graph | devspace | seeded | from MASTER seed |
| k06 | cert near-expiry sim — observe rotation | devspace | seeded | from MASTER seed |
| k07 | Temporal workflow inspect — find a stuck activity in the UI | devspace | seeded | from MASTER seed |
| k08 | grpcurl `GetManagedHostNetworkConfig` and identify every field | devspace | seeded | topic 01-hbn §4 |
| k09 | mock dpu-agent reports stale `instance_network_config_version`; watch state machine refuse to advance | devspace | seeded | topic 01-hbn §4 |
| k10 | flip `MachineQuarantineState` in forgedb; confirm `BlockAllTraffic` ACLs would apply | devspace | seeded | topic 01-hbn §4 |
| k11 | `tcpdump` underlay between two DPU loopbacks; identify VXLAN/EVPN headers (lab) | lab | seeded | topic 01-hbn §4 (stretch) |
