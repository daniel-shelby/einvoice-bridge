# einvoice-bridge

A local-first middleware that sits between a Point-of-Sale (POS) and the
**Malaysian LHDN MyInvois** e-invoicing platform.

The POS hands an invoice to the bridge over HTTP. The bridge persists it to a
local SQLite database immediately, returns a `202 Accepted`, and then a
background worker handles UBL document construction, RSA-SHA256 signing,
submission to LHDN, and status reconciliation — with retries and the
LHDN-mandated 72h cancellation window tracked per row.

If LHDN is unreachable, the POS is unaffected: the row stays `Pending` and is
retried with backoff. Nothing is lost.

## Why this exists

LHDN MyInvois requires every B2B invoice (and B2C invoices over RM10,000 once
the threshold takes effect) to be signed and validated against the government
endpoint. Calling LHDN synchronously from a POS is fragile — outages and
latency directly hit the checkout flow. This bridge decouples the two.

## Architecture

Hexagonal — domain logic (UBL construction, canonicalisation, signing) is
pure Rust with no IO and is fully testable from fixtures. Adapters wrap
SQLite, the LHDN HTTP API, and the inbound Axum API.

```
crates/
├── domain/    # pure: UBL builder, RFC 8785 canonicalisation, RSA signer
├── adapters/  # SQLx repository, LHDN HTTP client, Axum routes
└── bin/       # main.rs — wiring + background workers
migrations/    # SQLx migrations (committed)
.sqlx/         # offline query metadata (committed; needed for CI builds)
```

### Stack

| Concern        | Choice |
|----------------|--------|
| Runtime        | Tokio |
| HTTP server    | Axum 0.8 |
| HTTP client    | reqwest (rustls) |
| Database       | SQLite via SQLx 0.8 (`runtime-tokio-rustls`) |
| Crypto         | `rsa` + `sha2` + `p12` (no system OpenSSL) |
| XML / JSON     | `quick-xml` for UBL, `serde_json` for the POS payload |
| Logging        | `tracing` |

`rustls` is used everywhere TLS is needed so the project builds cleanly on
Apple Silicon without fighting Homebrew OpenSSL linkage.

## API surface (v1)

| Method | Path                          | Purpose |
|--------|-------------------------------|---------|
| POST   | `/v1/invoices`                | Enqueue an invoice. Returns `202` with internal id and `Pending` status. |
| GET    | `/v1/invoices/:ref`           | Current state, plus `qr_url` once `Valid`. |
| POST   | `/v1/invoices/:ref/cancel`    | Request cancellation (only valid within 72h of LHDN validation). |
| GET    | `/healthz`                    | DB + LHDN OAuth token sanity. |

All LHDN traffic happens off the request path, in background workers
(`submitter`, `poller`, `canceller`).

## Prerequisites

- **Rust** 1.85+ (Edition 2024). Install via [rustup](https://rustup.rs).
- **sqlx-cli** for migrations and offline query prep:
  ```sh
  cargo install sqlx-cli --no-default-features --features rustls,sqlite
  ```
- An LHDN MyInvois **preprod** account, OAuth client credentials, and your
  signing certificate (`.p12`). You can request preprod access via the
  MyInvois portal.

## Configuration

Copy the example file and fill it in:

```sh
cp .env.example .env
```

| Variable | Description |
|----------|-------------|
| `DATABASE_URL` | SQLite URL, e.g. `sqlite:./data/invoices.db` |
| `LHDN_ENV` | `preprod` or `prod` |
| `LHDN_CLIENT_ID` / `LHDN_CLIENT_SECRET` | OAuth2 client credentials from MyInvois |
| `LHDN_P12_PATH` | Path to your signing certificate (`.p12`) |
| `LHDN_P12_PASSWORD` | Password for the `.p12` |
| `LHDN_TIN` | Your taxpayer TIN |
| `LHDN_BRN` | Your business registration number |
| `BIND_ADDR` | HTTP listen address, e.g. `127.0.0.1:8080` |

The app fails fast at boot if any required variable is missing or if the
`.p12` cannot be decrypted.

## Running locally

```sh
# 1. Create the database file and apply migrations
mkdir -p data
sqlx database create
sqlx migrate run

# 2. Run the bridge (starts HTTP server + background workers)
cargo run -p einvoice-bridge
```

The server listens on `BIND_ADDR` (default `127.0.0.1:8080`).

### Submitting a test invoice

```sh
curl -X POST http://127.0.0.1:8080/v1/invoices \
  -H 'content-type: application/json' \
  -d @samples/invoice-001.json
```

Response:

```json
{ "id": "0193...", "invoice_ref": "INV-001", "status": "Pending" }
```

Poll for status:

```sh
curl http://127.0.0.1:8080/v1/invoices/INV-001
```

Once the worker has signed and submitted the document and LHDN has validated
it, the response includes `qr_url` and `lhdn_uuid`.

## Development

```sh
# Run the test suite (unit + integration)
cargo test

# Regenerate offline SQLx query metadata after editing any sqlx::query! macro
cargo sqlx prepare --workspace

# Lint
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Commit the `.sqlx/` directory — CI (and any teammate without a live database)
relies on it to compile the `sqlx::query!` macros offline.

## Project status

Pre-alpha. See [PLAN.md][plan] (or the conversation that generated this
project) for the full build order. Currently implemented:

- [x] Workspace scaffold + migrations + `.env.example`
- [ ] Domain: canonicalisation + RSA-SHA256 signer
- [ ] SQLx repository + `POST /v1/invoices`
- [ ] LHDN HTTP client + OAuth token cache
- [ ] Submitter worker (end-to-end against preprod)
- [ ] Poller + cancel + QR URL
- [ ] Hardening: retries, graceful shutdown, `/healthz`, observability

[plan]: ./PLAN.md

## Out of scope for v1

- Multi-tenant certificate stores
- Consolidated, self-billed, and refund-note document types
- Webhook callbacks to the POS (polling only)
- Prometheus metrics (structured `tracing` only)
