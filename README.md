# linkSDK

Embedded P2P networking and local-first shared data for personal and household applications.
The initial Rust core has no Tauri, React, terminal, desktop-media or TUN dependency.

## Quick start

Prerequisites: Rust/Cargo, Node.js (native-source preparation only), Git, CMake, a C/C++ toolchain and libclang for bindgen. Initial preparation downloads pinned native dependencies. The examples use a private Planet and do not require a third-party control service at runtime.

```sh
node scripts/prepare-libzt.mjs
cargo test --locked
cargo run --locked --example p2p-echo
cargo run --locked --example shared-kv
```

Each example starts its own two processes, generates distinct identities, creates a private space, explicitly approves the member through a local owner channel, runs assertions, and stops its children. Reports and retained profiles are under `artifacts/`. They never read an existing B5533/Link profile.

On a machine with VPNs or several network interfaces, select an IP actually reachable by both test processes:

```sh
LINKSDK_TEST_ROOT_IP=192.168.1.10 cargo run --locked --example p2p-echo
LINKSDK_TEST_ROOT_IP=192.168.1.10 cargo run --locked --example shared-kv
```

Replace the address with this machine's real interface address. A network change during a run can invalidate the signed root endpoint; start a new isolated run afterward. Do not change system routes or VPN settings just to obtain a passing result.

Data-only users do not need libzt sources, CMake, Node, or bindgen:

```sh
cargo test --locked --no-default-features --features data
```

```rust,no_run
use link_sdk::{runtime::Sdk, SpaceConfig};
use serde_json::json;

let sdk = Sdk::open_replica("my-local-profile", SpaceConfig { network_id: 42 })?;
let notes = sdk.dataset("my-app.v1", "notes");
notes.put("welcome", json!({"text": "saved locally"}))?;
let entry = notes.get("welcome")?;
# Ok::<(), String>(())
```

Only use a real trusted space ID for a replica that will later join a network. Data writes acknowledge local durability, not delivery. Replication is explicit through `data::replication::sync_once`; the host schedules repair and retries. See [design and validation status](docs/design.md) for scope, trust assumptions and lifecycle rules.

This is private v0.2.0. B5533 already consumes earlier immutable SDK revisions; adopting this release and fixing proxy failure policy remains a separate consumer change. macOS local-process validation does not establish Windows/Linux/mobile, remote NAT, or long-running reliability support.

## v0.2.0 data resilience

Typed data operations use `data::{DataStore, Operation, DataErrorKind}`. Read-only multi-dataset snapshots, explicit stale/failure events and bounded coalescing subscriptions are available without starting networking. Existing String entry points remain compatible. See the [Link application handoff](docs/link-app-handoff-v0.2.0.md) for API usage, error policy and a concrete consumer regression plan.

```sh
cargo run --locked --no-default-features --features data --example data-resilience
cargo test --locked --no-default-features --features data --test data_contention
```

These use temporary local data only. The example should report `result=passed`, prove reads remain available under a real SQLite write lock, reject the blocked write without acknowledging it, then recover after unlock. The SDK does not decide whether an application should keep running or extend its authorization. This private SDK is distributed through GitHub tag/release `v0.2.0`, not crates.io.
