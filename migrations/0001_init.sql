-- Billing's only table. One row per (consumer, external_reference): a
-- caller's retried CreatePaymentIntent finds and returns the existing row
-- instead of creating a second Stripe charge for the same thing.
--
-- Money is stored in minor units (cents) as BIGINT. No floats anywhere
-- near a charge.
CREATE TABLE IF NOT EXISTS payment_intents (
  id                       BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  stripe_payment_intent_id VARCHAR(128)    NOT NULL,
  -- Who asked, and what they're calling it -- opaque to Billing itself.
  -- e.g. consumer = "domain_management", external_reference = a
  -- domain_orders.id. Billing never interprets either string.
  consumer                 VARCHAR(64)     NOT NULL,
  external_reference       VARCHAR(191)    NOT NULL,
  amount_cents             BIGINT          NOT NULL,
  currency                 CHAR(3)         NOT NULL,
  -- A raw Stripe PaymentIntent status string (see src/stripe/mod.rs), kept
  -- as the source of truth locally so GetPaymentIntent/WatchPaymentIntent
  -- don't have to call Stripe on every read.
  status                   VARCHAR(32)     NOT NULL,
  last_error               TEXT            NULL,
  created_at               TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at               TIMESTAMP       NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  UNIQUE KEY payment_intents_consumer_ref (consumer, external_reference),
  UNIQUE KEY payment_intents_stripe_id (stripe_payment_intent_id),
  KEY payment_intents_status (status)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
