# Quality assurance and quality control

**QA** asks "does the product do what a user expects?" and is answered by scenario tests that drive
the shipped artifacts (the UI bundle in a real browser engine, the `peervps` binary over HTTP).
**QC** asks "is every build fit to merge?" and is answered by gates CI runs on every commit, on Linux
and macOS, which all must pass.

## Test suites

| Suite | Kind | Where | What it proves |
|---|---|---|---|
| Rust unit tests (47) | QC | `crates/*/src/**` `#[cfg(test)]` | Tunnel codec, Noise handshake, STUN, hole punching, routing, failover detection, hibernation sealing, replication, SLA slashing, ledger/meter/collateral/webhooks, Firecracker backend against a fake API server, interface-name rules, host telemetry probes |
| CLI acceptance (2–3) | QA | `crates/peervps-cli/tests/acceptance.rs` | Starts the real `peervps serve` binary and walks an agent through offers → deploy → status → per-second billing → scale 0/1 → terminate; rejects a bad API key (401), unknown offer and instance (404); on macOS refuses the Firecracker backend with a clear message |
| UI unit tests (21) | QC | `apps/desktop/src/**/*.test.ts(x)` | Formatting, the browser mock bridge (offers filter, lifecycle, typed errors, top-ups, failover timing, metering), the 60 fps event store (history caps, once-per-frame flush, routes, balances), app navigation and the main user flows in jsdom |
| UI end-to-end (6) | QA | `apps/desktop/e2e/app.spec.ts` | In Chromium (Linux) and WebKit (macOS, same engine as the app's WKWebView): live telemetry updates, deploy + xterm shell + scale + terminate, offer filters, wallet top-up reaching the ledger, failover reroute after three missed heartbeats and recovery, every view fitting the 1024×680 minimum window. Any console error or warning fails the test |
| macOS app smoke test | QA | `macos-app` CI job | Builds the `.app`/`.dmg`, checks the bundle, launches it and verifies the node starts and keeps running |

## CI gates (`.github/workflows/ci.yml`)

Run on `ubuntu-24.04` and `macos-15` for every pull request and every push to `main`:

1. `pnpm build`: TypeScript strict typecheck + production bundle
2. `pnpm test:coverage`: UI unit tests; fails below 70 % lines, 65 % functions, 60 % branches
3. `pnpm test:e2e`: Playwright QA scenarios (Chromium on Linux, WebKit on macOS)
4. `cargo fmt --all --check`
5. `cargo clippy --workspace --all-targets -- -D warnings` with `unsafe_code = "deny"` workspace-wide (the KVM module is the only audited exception)
6. `cargo test --workspace`: unit + acceptance tests
7. `macos-app` (after the above): unsigned Apple-silicon `.dmg`, launch smoke test, uploaded as the `PeerVPS-macos-arm64` artifact

Playwright reports, failure traces and the coverage report are uploaded as `qa-report-<os>` on every run.

## Running locally

```bash
pnpm install && pnpm build
pnpm test                 # or pnpm test:coverage
pnpm test:e2e             # all browsers; add --project chromium or --project webkit
cargo test --workspace
```

## Results for this change (Linux sandbox, 2026-10-08)

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

## Not covered yet

* Real MicroVM boot on `/dev/kvm` and on macOS (needs a Virtualization.framework backend); the Firecracker backend is tested against a fake API server.
* TUN device creation (needs root / `CAP_NET_ADMIN`).
* Visual regression baselines and load/performance testing of the event pump.
