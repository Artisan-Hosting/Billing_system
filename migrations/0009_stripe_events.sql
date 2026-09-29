-- Webhook inbox: one row per Stripe event id ever received. Stripe delivers
-- at-least-once and may retry or reorder, so the handler records an event
-- before acting on it and skips one already marked processed. `processed_at`
-- stays NULL if the handler crashed or errored part-way; Stripe's retry then
-- re-runs it, which is safe because every effect it has is itself idempotent
-- (ledger writes carry a unique idempotency key).
CREATE TABLE IF NOT EXISTS stripe_events (
  event_id     VARCHAR(191) NOT NULL PRIMARY KEY,
  event_type   VARCHAR(128) NOT NULL,
  received_at  TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  processed_at TIMESTAMP    NULL,
  KEY stripe_events_type (event_type, received_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
