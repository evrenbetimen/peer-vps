# Quality assurance and quality control

**QA** asks "does the product do what a user expects?" and is answered by scenario tests that drive
the shipped artifacts (the UI bundle in a real browser engine, the `peervps` binary over HTTP).
**QC** asks "is every build fit to merge?" and is answered by gates CI runs on every commit, on Linux
and macOS, which all must pass.

## Test suites

| Suite | Kind | Where | What it proves |
|---|---|---|---|
| Rust unit tests (74) | QC | `crates/*/src/**` `#[cfg(test)]` | Tunnel codec, Noise handshake, STUN, hole punching, routing, failover detection, hibernation sealing, replication, SLA slashing, ledger/meter/collateral/webhooks, Firecracker backend against a fake API server, interface-name rules, host telemetry probes, ISO import and labels, the Windows answer file, a real QEMU booting a Windows-labelled ISO with a password-locked screen, the Noise XX peer channel, two nodes on localhost renting from each other (approval, remote deploy, SSH carried through the channel, scale, terminate), pinned-key and wrong-id refusals, node key and peer list persistence, LAN beacons (and forged ones dropped), UPnP port forwarding against a fake router (port conflicts, permanent-lease routers, CGNAT and double NAT detected, public machines), STUN-confirmed public address |
| CLI acceptance (2–3) | QA | `crates/peervps-cli/tests/acceptance.rs` | Starts the real `peervps serve` binary and walks an agent through offers → deploy → status → per-second billing → scale 0/1 → terminate; rejects a bad API key (401), unknown offer and instance (404); on macOS refuses the Firecracker backend with a clear message |
| UI unit tests (30) | QC | `apps/desktop/src/**/*.test.ts(x)` | Formatting, the browser mock bridge (offers filter, lifecycle, typed errors, top-ups, failover timing, metering), the 60 fps event store (history caps, once-per-frame flush, routes, balances), app navigation, the main user flows, SSH access lines, image downloads, and adding a Windows ISO then deploying it with a screen and Remote Desktop in jsdom, adding and approving peers and renting a peer's offer, adding a machine found on the network, internet reachability on and off |
| UI end-to-end (11) | QA | `apps/desktop/e2e/app.spec.ts` | In Chromium (Linux, Windows) and WebKit (macOS, same engine as the app's WKWebView): live telemetry updates, deploy + xterm shell + scale + terminate, offer filters, wallet top-up reaching the ledger, failover reroute after three missed heartbeats and recovery, guest image download, the SSH command shown for a new instance, a Windows ISO added and deployed with Remote Desktop and a screen, adding and approving peers and deploying on a peer, adding a nearby machine and opening the router port, every view fitting the 1024×680 minimum window. Any console error or warning fails the test |
| QEMU backend | QA | `virtualization/qemu/tests.rs`, `qmp.rs`, `seed.rs`, `images.rs` | Command line per accelerator and OS, QMP protocol, cloud-init seed, snapshot framing, image catalog and checksums; plus a lifecycle test that drives a real QEMU (create → start → pause → snapshot → resume → restore → destroy) wherever QEMU is installed (CI: Linux, macOS) |
| Real guest boot | QA (manual, recorded below) | `peervps serve --hypervisor qemu` | Ubuntu 24.04 cloud image boots, cloud-init creates the user, SSH with key and sudo work, scale to zero and back keeps the session's files |
| App smoke tests | QA | `macos-app`, `windows-app` CI jobs | Build the installers, launch the app and verify the node starts and keeps running |

## CI gates (`.github/workflows/ci.yml`)

Run on `ubuntu-24.04`, `macos-15` and `windows-2025` for every pull request and every push to `main`:

1. `pnpm build`: TypeScript strict typecheck + production bundle
2. `pnpm test:coverage`: UI unit tests; fails below 70 % lines, 65 % functions, 60 % branches
3. `pnpm test:e2e`: Playwright QA scenarios (Chromium on Linux and Windows, WebKit on macOS)
4. `cargo fmt --all --check`
5. `cargo clippy --workspace --all-targets -- -D warnings` with `unsafe_code = "deny"` workspace-wide (the KVM module is the only audited exception)
6. `cargo test --workspace`: unit + acceptance tests
7. `macos-app` and `windows-app` (after the above): unsigned `.dmg` and NSIS installer, launch smoke test,
   uploaded as the `PeerVPS-macos-arm64` and `PeerVPS-windows-x64` artifacts

Playwright reports, failure traces and the coverage report are uploaded as `qa-report-<os>` on every run.

## Running locally

```bash
pnpm install && pnpm build
pnpm test                 # or pnpm test:coverage
pnpm test:e2e             # all browsers; add --project chromium or --project webkit
cargo test --workspace
```

## Results when the QA suites were introduced (Linux sandbox, 2026-10-08)

| Gate | Result |
|---|---|
| Typecheck + build | pass |
| UI unit tests | 21/21 pass; coverage 85.7 % lines, 78.2 % functions, 65.7 % branches, 83.6 % statements |
| UI end-to-end (Chromium) | 6/6 pass; 30/30 across five repeated runs (flakiness check) |
| `cargo fmt`, `cargo clippy -D warnings` | clean |
| `cargo test --workspace` | 47 unit + 2 acceptance pass |
| macOS gates, WebKit, `.dmg` | run by CI on `macos-15` |

Defects found and fixed while writing these tests:

* **Wallet billing log never updated in the browser preview.** The mock bridge returned its live
  objects, so React saw the same reference after a top-up and skipped the re-render. The mock now
  returns copies, as real IPC does.
* **Host telemetry was Linux-only.** CPU, RAM, network and temperature were read from `/proc` and
  `/sys`, so a Mac showed zeros and a made-up 16 GiB. They now come from `sysinfo` on both platforms.
* **No accessible state on navigation and pickers.** Added `aria-current` to the active view and
  `aria-pressed` and group labels to the template and size pickers, which the tests (and screen readers) rely on.

## Real VM run for the QEMU backend (Linux sandbox, software emulation, 2026-10-08)

`peervps image pull ubuntu-24.04` (625 MB, SHA-256 verified) → `peervps serve --hypervisor qemu --ssh-key k.pub`
→ `peervps deploy fra-cpu-1 --vcpus 2 --mem-mib 2048 --disk-gib 10 --image ubuntu-24.04`:

* cloud-init finished from the PeerVPS seed (`DataSourceNoCloudNet [seed=dmi,http://10.0.2.2:…]`) after 140 s without acceleration
* `ssh -p <port> peervps@127.0.0.1` with the key: Ubuntu 6.8 kernel, 2 vCPU, 1.9 GiB RAM, root grown to 8.7 GiB, `sudo -n whoami` → `root`
* `peervps scale <id> 0` took 8.6 s (2 GiB RAM snapshot); `scale <id> 1` resumed and the file written before was still there
* `peervps terminate` stopped QEMU and removed the VM directory

## Two nodes renting from each other (Linux sandbox, software emulation, 2026-10-08)

Node B: `peervps serve --listen 127.0.0.1:7170 --peer-listen 127.0.0.1:7171 --hypervisor qemu` with a CirrOS
0.6.2 disk imported as `cirros`. Node A: `peervps serve --listen 127.0.0.1:7080 --peer-listen 127.0.0.1:7081`
(mock hypervisor, so nothing can run on A itself).

* `peervps --api A peer add pv-…@127.0.0.1:7171` → `waitingForApproval`; B listed A as `pending` with its dial-back address
* `peervps --api B peer approve <A>` → A listed `pv-…/this-machine` among its offers within one refresh
* both nodes restarted: the keys and peer lists were reloaded and the peers came back `online` without re-approval
* `peervps --api A deploy pv-…/this-machine --image cirros` → QEMU started on B; `peervps --api A access` gave
  `ssh -p <local port> …@127.0.0.1` on A
* through that port: the guest's `SSH-2.0-dropbear` banner, then `ssh cirros@127.0.0.1 -p <port>` logged in and
  ran `uname` (`Linux 5.15.0-71-generic`, `QEMU TCG CPU`)
* `peervps --api A terminate` stopped QEMU on B; B's ledger showed the `peer-<A>` account with its welcome credit
* two nodes started on `0.0.0.0` each listed the other under `nearby` within one beacon interval; `--upnp` without a router reported `noGateway` with the port to forward by hand

## Not covered yet

* Hardware-accelerated runs (KVM / Hypervisor.framework / WHPX): the sandbox and CI runners have no nested virtualization, so these use software emulation. QEMU on Windows is not installed in CI, so the Windows lifecycle test skips.
* Firecracker on real `/dev/kvm`; it is tested against a fake API server.
* TUN device creation (needs root / `CAP_NET_ADMIN`).
* UPnP against real routers: tested against a fake gateway only (the sandbox has no router; `serve --upnp` there reports `noGateway`). Two machines that are both behind CGNAT cannot connect until a relay exists.
* Visual regression baselines and load/performance testing of the event pump.
