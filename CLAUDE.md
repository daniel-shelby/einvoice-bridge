# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`einvoice-bridge` is a local-first Rust middleware between a Malaysian POS and the LHDN MyInvois e-invoicing platform. The POS gets a `202 Accepted` immediately; background workers handle UBL build, RSA-SHA256 signing, submission, polling, and cancellation. See `PLAN.md` for the full build order and `README.md` for the user-facing description.

## Commands

```sh
# Build / test the whole workspace
cargo build --workspace
cargo test --workspace

# Run a single test (integration tests live under crates/adapters/tests/*.rs)
cargo test -p einvoice-adapters --test worker happy_path_submits_persists_and_clears_outbox
cargo test -p einvoice-domain canonicalize::

# Lint + format (CI gate)
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings

# Run the binary (needs .env with LHDN creds, OR set LHDN_OFFLINE=true)
cargo run -p einvoice-bridge

# Migrations + offline SQL metadata (sqlx-cli)
sqlx database create
sqlx migrate run
cargo sqlx prepare --workspace   # rerun after editing any sqlx::query! macro
```

`.sqlx/` is committed and required for offline compilation of the `sqlx::query!` macros (CI builds without a live DB).

## Architecture

Hexagonal, three crates:

- **`crates/domain/`** — pure, no IO, no async. Owns the LHDN two-stage signing flow:
  build unsigned UBL → canonicalise (RFC 8785 via `serde_jcs`) → SHA-256 → RSA-SHA256 sign →
  embed `UBLExtensions` + `Signature` (with cert digest, signing time) → re-canonicalise →
  final `documentHash`. Public surface: `Signer`, `build_signed_document(payload, signer, signing_time) -> SignedDocument`.
  `#![recursion_limit = "256"]` is required for the `serde_json::json!` macro in `ubl.rs`.

- **`crates/adapters/`** — every IO concern. One module per concern, none of which leak across:
  - `api.rs` — Axum router. Submit handler is **DB-only** (insert + outbox event in one tx → 202). No LHDN call on the request path.
  - `repo.rs` — `InvoiceRepo` over SQLx. Both API methods (`create_pending`, `find_by_ref`) and worker methods (`due_*_events`, `complete_submission`, `fail_permanently`, `reschedule`). All queries are `sqlx::query!`-checked at compile time.
  - `lhdn/` — `LhdnClient` (rustls-only `reqwest`), in-memory + DB token cache with single-flight refresh (`Mutex` + `RwLock`), four endpoints (submit/details/cancel/validate-taxpayer). Typed `LhdnError` with `is_transient()` driving retry policy. **OAuth `/connect/token` uses RFC 6749 snake_case**; LHDN business endpoints use camelCase — do not unify the rename rule across `TokenResponse` and the rest.
  - `worker.rs` — `Submitter` background loop. `tick()` pulls due `outbox_events`, builds + signs the UBL, calls LHDN, then `complete_submission` / `fail_permanently` / `reschedule`. Backoff: 30s / 2m / 10m / 1h capped, max 8 attempts. Per-event errors are logged but do not abort the tick.

- **`crates/bin/`** — `main.rs` wires SqlitePool → migrations → repo → Axum router → optional `Submitter` task → ctrl-c shutdown via `tokio::sync::watch`. `LHDN_OFFLINE=true` skips loading the `.p12` and the worker — useful for dev without preprod creds.

### Lifecycle (durable outbox)

`invoices.lhdn_status` ∈ `Pending | Submitted | Valid | Invalid | Cancelled | Failed`. Workers are driven by `outbox_events(kind ∈ submit|poll|cancel, available_at)`. Times are `INTEGER` unix seconds. The submitter inserts on `create_pending`; subsequent kinds (`poll`, `cancel`) follow the same pattern.

### Testing

- Unit tests live next to source (domain canonicalisation, signer round-trip, backoff math).
- Integration tests are under `crates/adapters/tests/{api,lhdn,worker}.rs`. They use `sqlite::memory:` with `max_connections(1)` (single-writer keeps SQLite happy) + `wiremock` for LHDN. The `migrations/` are applied per test pool.
- Worker tests drive `submitter.tick()` directly — no real time, no real network.

### Conventions worth knowing

- **Hexagonal boundary is strict.** Domain stays IO-free. New IO goes in `adapters/`.
- **Compile-time SQL.** Use `sqlx::query!` / `sqlx::query_as!`; rerun `cargo sqlx prepare --workspace` and commit `.sqlx/`.
- **Times are unix seconds (i64) end-to-end** in the DB layer; convert at the edges with `time::OffsetDateTime`.
- **Errors:** `thiserror` with `#[from]` for typed source chaining (`LhdnError`, `RepoError`, `ApiError`). Distinct variants exist where retry policy differs (`is_transient()`).
- **No system OpenSSL.** Everything TLS uses rustls (`reqwest` `rustls-tls`, `sqlx` `runtime-tokio-rustls`). Crypto is pure-Rust (`rsa`, `sha2`, `p12`).
- **Worker correctness invariants:**
  - Domain-validation failures (build_doc) call `fail_permanently` with the existing `attempts` (not bumped) — the row never reached LHDN.
  - LHDN call failures bump `attempts` and either `reschedule` (transient + under cap) or `fail_permanently` with the new count.
  - `complete_submission` / `fail_permanently` / `reschedule` are each one transaction that updates the invoice **and** removes/updates the outbox event together — never half-applied.
- **Defaults:** silent fallbacks in `build_unsigned_invoice` (missing fields → empty strings, currency defaults to `MYR`). Step 5b will tighten this against the real preprod sandbox; don't add validation gates before then.
