# linkSDK design and implementation record

## Scope

One public Rust crate, `link-sdk` (`link_sdk` in code). `native/libzt-sys` is an internal raw-binding/build dependency. Minimum core: OS support, P2P networking and shared data. `runtime` composes them. Application-specific PTY, process execution, TUN, media and UI remain outside this repository.

The GitHub repository is private: `ZevaierZhan/linkSDK`. Public distribution and a license for original SDK code have not been selected; retain third-party notices. No crates.io publication is configured.

## Modules and dependency rules

- `platform`: profile ownership lock, private files and identity-format support. Use Rust's existing platform facilities instead of wrapping every standard-library operation.
- `network`: pinned libzt, private Planet/Root/embedded Controller, stable node identity, explicit local organizer approval, trusted roster and bounded TCP streams. No official Planet fallback.
- `data::store`: extracted Automerge/SQLite register implementation. No libzt or network-runtime dependency. Data-only builds work without `vendor/`.
- `data::replication`: explicit Automerge repair over libzt TCP; bounded framing and transactions retained from the original implementation. It consumes network connections and a trusted roster.
- `runtime::Sdk`: local profile ownership and dataset construction; network `Node` owns its SDK lifetime.

Core code must not import B5533 Runtime or applications. One active libzt lifecycle/space per process is supported in 0.1. Use a new process for a stopped or failed node; in-process libzt reinitialization is not promised.

## Trust and membership

Bootstrap material must arrive through a trusted out-of-band channel. `Bootstrap` is not a secure invitation workflow. The examples generate identities independently and explicitly approve the leaf ID through organizer stdin; there is no remotely exposed management endpoint. Approval is read back from the Controller.

The host supplies a trusted mapping of node ID to controller-assigned virtual IP with `trust_peer`. Incoming sync and echo check their envelope space/source/target against that roster and source IP. This retains the original honest-authorized-member trust model; it does not add per-message authorship proofs or protection against a malicious member rewriting shared business data. A namespace is not an ACL. Shared datasets are available to all trusted participating members. Do not store secrets requiring per-member confidentiality in them.

Root/Controller are still single-organizer roles. No automatic failover, dynamic Root promotion, multi-space participation or global immediate revocation has been implemented. The organizer's signed root endpoint must remain reachable. Current minimal roster is supplied by the owner; dynamic authenticated discovery and a secure invitation UX are later work.

## Data contract

Atomic JSON values indexed by space/app/dataset/key. Nested JSON fields are not merged separately. Automerge 0.12.0 owns causality; SQLite FULL/WAL owns local durability. Successful writes return `saved=local, replicated=false`. Concurrent versions remain inspectable; a later write resolves observed versions. Deletion retains causal history and is not guaranteed to defeat an unseen concurrent write.

Default total history quota: 8 MiB / 128 datasets; existing storage supports expansion up to 64 MiB / 1024 datasets, with 64 KiB maximum single JSON value. Deletion does not reclaim history. There is no history GC or recoverable-backup facility. Network incrementality does not mean SQLite incrementality. Application schema incompatibility requires a new versioned namespace and explicit business migration.

Wire protocol remains v2 and SQLite format v1. Format, protocol and SDK package versions are independent. Unknown future storage formats must fail before mutation. Actor incarnation survives restarts. Profiles must never be copied and activated as two independent nodes. Dataset handles retain profile-lock ownership even when the original Sdk handle is dropped.

The extracted Store no longer reads the network identity to perform local operations. Its stable actor belongs to the database; Replication supplies the live network identity in its envelope. Original storage schema and causal history layout are retained. Compatibility with an actual running B5533 build is not yet verified.

## Lifecycle and transport

Callers start the network explicitly. Local data access does not start it. Network configuration readiness is distinct from application reachability. `sync_once` is an explicit repair cycle; the host chooses scheduling and retry behavior. No automatic full-mesh repair loop or availability promise is hidden in this first extraction.

Close streams and stop/join application servers and `ReplicaServer` before shutting down the Node. ReplicaServer borrows Node and joins its worker on drop. Stream handles expose bounded frames and deadline-aware reads/writes. Close cancels operations without racing lwIP's non-full-duplex receive mailbox. Raw libzt ABI counts are checked before slicing buffers, including Windows zero-extended negative ssize_t values. Underlying sockets and directory ownership are not process-global application registries.

This initial public transport surface is TCP. libzt retains UDP support internally; a public UDP wrapper and its real datagram acceptance remain follow-up work. Native TLS IO adaptation is retained for later authenticated control protocols; current example echo relies on libzt plus the explicit roster checks.

## Native build

`native/dependencies.json` pins source revisions and source hashes. `scripts/prepare-libzt.mjs` retrieves upstream and submodules, applies checked patches, and builds a native static library. Generated sources/builds stay in ignored `vendor/`. The SDK's tracked internal FFI crate makes data-only Cargo resolution independent of that generated directory. Node.js is currently a build-preparation dependency, not a runtime dependency.

The initial implementation copied a local source cache to avoid a temporary network outage, then built the native archive from source in the new repository. It did not copy the old application's archive into the SDK. A subsequent GitHub fresh clone downloaded pinned upstream sources and submodules, applied the patches, and rebuilt successfully without the old workspace. Android cross-target binding code is included but unverified; complete Android native preparation and iOS SDK/signing/packaging are not delivered here. Do not reuse a host archive for another target; an explicit `LINKSDK_LIBZT_LIB_DIR` can identify a separately prepared target archive.

## Examples and acceptance

`p2p-echo`: two distinct processes and identities; self-hosted private Planet; explicit organizer approval and roster binding; bidirectional real libzt TCP echo; orderly shutdown. Node-only build is supported.

`shared-kv`: the same bootstrap, then baseline synchronization; stop B; write B's local database with no network runtime while A writes a concurrent value; restart B with its persistent identity/port; repair via real libzt TCP; require equal heads and both concurrent versions; resolve the observed conflict; sync again; stop both processes and reopen B offline to verify persisted results. It preserves run profiles and negative reports under artifacts.

Local tests do not cover real machines losing power, disks failing, remote NAT, long partitions, malicious members, Windows/Linux runtime behavior or mobile lifecycle. Compilation and local-process results must not be promoted into those claims.

## Initial implementation and handoff

Extracted from the working source of `ZevaierZhan/link-personal-assets` (base commit recorded in native/dependencies.json; source had uncommitted changes). Imported socket/frame/address code, shared-data Store/sync protocol and its failure tests, and reproducible native patches. Removed the application Runtime dependency and terminal-local in-memory transport. Added minimal node/bootstrap/approval, SDK profile ownership and standalone example orchestration. The original app and formal household space were not modified or restarted.

Negative evidence retained: initial shell GitHub access failed; after network restoration repository creation and `isPrivate=true` read-back succeeded. First echo attempt used the previous interface IP after a network change and leaf startup timed out; using the current physical address passed. First shared-kv restart exposed a readiness bug: stored-network restoration emitted READY before explicit join and the new wrapper cleared it. The wrapper now retains the event. A subsequent immediate repair observed a connection deadline while the restarted peer path settled; example recovery now keeps the same physical port and retries explicit repair within a bounded 60-second window rather than equating configuration readiness with connectivity. Final run verdicts are recorded after verification below.

Verified locally on macOS ARM64, rustc/cargo 1.99.0:

- Native archive rebuilt from source in this repository; locked revision/submodules/patch hashes checked (`artifacts/native-locked-build.log`).
- `cargo test --locked --offline`: 18 unit tests plus 3 public-contract tests passed (`artifacts/core-final.log`).
- Data-only build: 11 Store tests passed. With the entire vendor directory temporarily absent, the 3 public-contract tests also passed (`artifacts/data-only-sys.log`, `artifacts/data-without-vendor.log`). Vendor was restored after the probe.
- `p2p-echo` passed with two independent identities, private Planet, explicit approval, bidirectional real libzt TCP and orderly shutdown (`artifacts/p2p-echo-1792928128838-43179/report.json`).
- `shared-kv` passed all six acceptance checks, including offline write, concurrent version retention, reconnect convergence, conflict resolution and offline reopen after full shutdown (`artifacts/shared-kv-1792928333172-52840/report.json`).
- Formatting check passed. Negative runs remain in ignored artifacts; identities and keys are not committed.

## Fresh GitHub checkout verification, 2026-10-25

Initial code commit `202225045c8939da072c20f38be7d6faff7e5cf0` was pushed to private `ZevaierZhan/linkSDK/main`; repository privacy was read back as true. A fresh GitHub clone under `artifacts/fresh-clone` independently fetched libzt and its submodules plus the pinned JSON header, checked revisions/input hashes, applied patches to pristine sources, and compiled its own archive (`artifacts/fresh-native-build.log`). The preparation script was also rerun idempotently in the primary checkout.

In that fresh clone:

- `cargo test --locked --offline --no-default-features --features data --test public_contract`: 3 passed (`artifacts/fresh-data-only.log`).
- `cargo run --locked --offline --example p2p-echo`: passed, real two-process TCP in a new private space (`artifacts/fresh-clone/artifacts/p2p-echo-1792928769555-69716/report.json`).
- `cargo run --locked --offline --example shared-kv`: all six checks passed, including offline concurrent modification, restart and resolution persistence (`artifacts/fresh-clone/artifacts/shared-kv-1792928787031-70688/report.json`). Reconnection needed two transient connect-deadline retries within the bounded repair window; this is observed recovery, not zero-delay availability.
- Network-only `p2p-echo` compilation passed in the primary checkout (`artifacts/network-only-final.log`).

GitHub Actions run `40190038086` did not start the data runner: the check annotation states that recent account payments failed or the spending limit needs to be increased. There were no executed CI steps. The other annotation concerns the Ubuntu runner-image migration and is not the cause. Remote CI is therefore unverified, not a passing check or a code-test failure. Local test claims above remain limited to macOS ARM64.

User verification: clone the private repository, install prerequisites, run `node scripts/prepare-libzt.mjs`, then run both example commands from README. Each should end with a JSON report whose `result` is `passed`; shared-kv must include concurrent version retention and offline reopen checks. Supply `LINKSDK_TEST_ROOT_IP` if default interface selection is inappropriate. Profiles/reports are retained; do not upload their identity or Planet-signing key files.

Next: separately migrate B5533 to an immutable SDK commit and verify original format/protocol interoperability; add typed errors, richer lifecycle/status and automatic repair scheduling only as needed; then implement/test a public UDP surface, mobile build preparation and additional platform acceptance. No B5533 consumer migration, multi-language bindings, independent daemon, cross-device NAT or long-running reliability acceptance has been completed.
