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

This is a private 0.1 extraction. macOS local-process validation does not establish Windows/Linux/mobile, remote NAT, or long-running reliability support. B5533 has not yet migrated to consume this repository.
