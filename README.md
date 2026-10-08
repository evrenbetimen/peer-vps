# PeerVPS

Decentralized, agent-native compute network: anyone can rent out spare CPU/GPU/NPU capacity as
hardware-isolated MicroVMs, and humans or autonomous AI agents can find, deploy, scale and pay for them
programmatically. A ZeroTier-style encrypted P2P overlay connects clients straight to their guests.

This repository is the monorepo scaffold: a Rust node engine, a CLI/headless node for agents, and a
Tauri v2 desktop dashboard (React 19 + TypeScript + Tailwind CSS 4).

```
peer-vps/
├── Cargo.toml                    Rust workspace (lints: unsafe denied, clippy::all)
├── crates/
│   ├── peervps-core/             Node engine (library)
│   │   └── src/
│   │       ├── virtualization/   MicroVM provisioner, KVM backend, vGPU/NPU slices, TEE, proof of compute
│   │       ├── network/          zstd + ChaCha20-Poly1305 tunnel codec, Noise IK, STUN, hole punching, TUN, routing
│   │       ├── failover/         heartbeats, (A) hibernation, (B) block replication, (C) SLA slashing, Kademlia table
│   │       ├── storage/          SQLite (WAL) + versioned migrations, presence log
│   │       ├── billing/          µcredit ledger, per-second meter, collateral + pools, signed payment webhooks
│   │       ├── api/              REST /v1 for agents (axum) + marketplace filter
│   │       └── node.rs           façade wiring everything together
│   └── peervps-cli/              `peervps` CLI + `peervps serve` headless node
├── proto/peervps/v1/node.proto   gRPC contract mirroring the REST API
└── apps/desktop/                 Tauri v2 app
    ├── src/                      React UI: Host, Console (+ xterm), Wallet, Failover topology
    │   └── bridge/               typed `invoke` wrappers + 60 fps event store
    └── src-tauri/                commands, event pump, metrics sampler, failover demo topology
```

## What is real and what is a stub

| Area | Working today | Stubbed behind a trait (next steps) |
|---|---|---|
| Virtualization | Resource allocator with core pinning, RAM/disk budgets, fractional GPU/NPU slice accounting; KVM backend opens `/dev/kvm`, creates the VM, registers guest RAM, creates vCPUs | Kernel boot + vCPU run loop, pause/snapshot on KVM (or a Firecracker backend), VFIO passthrough, SEV-SNP/TDX launch and real attestation |
| Proof of compute | Nonce-bound sequential BLAKE3 hash-chain with spot-check verification and tier timing | Succinct ZK proof (zkVM receipt) behind the same `ComputeProver`/`ComputeVerifier` traits |
| Network | Tunnel codec (zstd → ChaCha20-Poly1305, replay window), Noise IK handshake (`snow`), STUN client/responder, UDP hole punching with port spraying, overlay routing table, Linux TUN pump | TURN relay fallback, DHT RPCs on the wire, QUIC snapshot transport |
| Failover | Authenticated heartbeats, 3-miss detection, route flip to standby, snapshot seal/open (zstd + chunked AEAD), SIGTERM/SIGINT hibernation, dirty-block replication, SLA slashing | logind shutdown inhibitor, replica restore path |
| Billing | Integer µcredit ledger with journal, per-second settlement (drift-free), platform fee, suspension on empty balance, collateral lock/unlock/slash, pooled staking with pro-rata slashing, HMAC-SHA256 webhooks with replay window and idempotency | Real payment provider integration (an `HttpGateway` skeleton exists) |
| API | REST `/v1` (offers, deploy, scale to zero, terminate, account, webhooks), CLI | tonic server for the `.proto` contract, event streaming |
| Desktop | All four views wired to the node through `invoke` and a frame-throttled event stream; the failover view drives the real `FailoverController` | SSH over the overlay (the terminal is a local echo shell for now) |

## Prerequisites

* Rust 1.85+ (edition 2024)
* Node 22 + pnpm 10
* Linux desktop builds: `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev`
  (see the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for macOS/Windows)

## Build, lint, test

```bash
pnpm install
pnpm build                                   # typecheck + bundle the UI (the Tauri crate embeds apps/desktop/dist)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Run

```bash
pnpm tauri dev                               # desktop app (Host / Console / Wallet / Failover)
pnpm dev                                     # UI only, in a browser, against mock data

cargo run -p peervps-cli -- serve            # headless node on 127.0.0.1:7070 (prints a demo API key)
export PEERVPS_API_KEY=pvps_demo_key
cargo run -p peervps-cli -- offers --min-vram-mib 10000 --max-price-per-hour 50
cargo run -p peervps-cli -- deploy fra-cpu-1 --vcpus 2 --mem-mib 4096
cargo run -p peervps-cli -- scale <instance-id> 0
cargo run -p peervps-cli -- account
```

## Safety rules

* `unsafe_code = "deny"` across the workspace. The only exception is `virtualization/kvm.rs`, which opts in
  explicitly and documents each block with a `// SAFETY:` comment (`clippy::undocumented_unsafe_blocks` is denied).
* Money is integer µcredits everywhere; every balance change is a journaled row inside one SQLite transaction,
  and balances have a `CHECK (balance >= 0)` constraint.
* Every network-facing message is authenticated: tunnel datagrams (AEAD), heartbeats (keyed BLAKE3),
  snapshots (per-chunk AEAD bound to VM id, index and count), webhooks (HMAC-SHA256 + timestamp).
