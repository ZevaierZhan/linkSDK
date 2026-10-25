# Development

Read `docs/design.md` before implementation. It is the single source of design decisions, current status and handoff notes. README is the operating index.

- Keep the public core independent of B5533, Tauri, React and application-specific OS capabilities.
- Preserve existing identities, Automerge actors, wire v2 and storage v1 during extraction. Change SDK, wire and storage versions separately.
- Native compatibility changes belong in `scripts/patch-libzt.mjs`, never only in generated `vendor/` sources.
- Never commit `vendor/`, `target/`, `artifacts/`, private identities, Planet signing keys or user profiles.
- Use isolated profiles and private test spaces. Stop only test-owned processes. Do not operate the user's installed applications or family space.
- Run checks appropriate to changes; record real results and limits in `docs/design.md`. Two-process local tests are not cross-device or five-platform acceptance.
- Baseline: `cargo test --locked --no-default-features --features data`; networking changes additionally require native preparation, `cargo test --locked`, and both examples.
