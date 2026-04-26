-- Initial schema for einvoice-bridge.
-- Times are stored as unix seconds (INTEGER) for portability with SQLite.

PRAGMA foreign_keys = ON;

CREATE TABLE invoices (
    id                  TEXT PRIMARY KEY,                     -- internal UUIDv7
    invoice_ref         TEXT NOT NULL UNIQUE,                 -- POS invoice number
    payload_json        TEXT NOT NULL,                        -- raw POS payload
    ubl_xml             TEXT,                                 -- generated, signed UBL document
    doc_digest          TEXT,                                 -- base64 SHA-256 of signed document
    signature           TEXT,                                 -- base64 RSA-SHA256
    lhdn_status         TEXT NOT NULL
                        CHECK (lhdn_status IN ('Pending','Submitted','Valid','Invalid','Cancelled','Failed')),
    lhdn_submission_uid TEXT,                                 -- batch-level id from LHDN
    lhdn_uuid           TEXT,                                 -- per-invoice uuid from LHDN
    long_id             TEXT,                                 -- needed to build the QR url
    qr_url              TEXT,
    error_json          TEXT,                                 -- last LHDN error envelope
    attempts            INTEGER NOT NULL DEFAULT 0,
    next_attempt_at     INTEGER,
    submitted_at        INTEGER,
    validated_at        INTEGER,
    cancellable_until   INTEGER,                              -- validated_at + 72h (LHDN window)
    cancelled_at        INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE INDEX idx_invoices_status_next
    ON invoices (lhdn_status, next_attempt_at);

CREATE TABLE oauth_tokens (
    env          TEXT PRIMARY KEY,                            -- 'preprod' | 'prod'
    access_token TEXT NOT NULL,
    expires_at   INTEGER NOT NULL
);

CREATE TABLE outbox_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    invoice_id   TEXT NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL
                 CHECK (kind IN ('submit','poll','cancel')),
    available_at INTEGER NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT
);

CREATE INDEX idx_outbox_due
    ON outbox_events (available_at);
