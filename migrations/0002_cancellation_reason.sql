-- Cancellation reason captured at the API boundary; the canceller worker
-- forwards it to LHDN when it processes the cancel outbox event.
ALTER TABLE invoices ADD COLUMN cancellation_reason TEXT;
