-- Invoices and their line items. `subscription_id` is nullable: a one-off
-- charge (a GPU credit top-up, a domain order billed through this path in
-- future) has no subscription behind it. Money in cents as BIGINT, same
-- discipline `payment_intents` already follows -- no floats near a charge.
CREATE TABLE IF NOT EXISTS invoices (
  id                       BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  organization_id          VARCHAR(36)  NOT NULL,
  subscription_id          BIGINT UNSIGNED NULL,
  period_start             TIMESTAMP    NOT NULL,
  period_end               TIMESTAMP    NOT NULL,
  -- draft | open | paid | void | uncollectible
  status                   VARCHAR(16)  NOT NULL DEFAULT 'draft',
  total_cents              BIGINT       NOT NULL DEFAULT 0,
  currency                 CHAR(3)      NOT NULL DEFAULT 'usd',
  -- Set once a PaymentIntent is created for this invoice through Billing's
  -- existing product-agnostic BillingService (consumer = "billing_invoices",
  -- external_reference = this invoice's own id) -- no new Stripe integration
  -- needed, this crate already owns both sides.
  stripe_payment_intent_id VARCHAR(128) NULL,
  created_at               TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at               TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  KEY invoices_org (organization_id, created_at),
  KEY invoices_status (status),
  UNIQUE KEY uq_invoices_stripe_pi (stripe_payment_intent_id),
  CONSTRAINT fk_invoices_subscription FOREIGN KEY (subscription_id) REFERENCES subscriptions (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS invoice_line_items (
  id                BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  invoice_id        BIGINT UNSIGNED NOT NULL,
  unit_code         VARCHAR(32)    NULL,
  description       VARCHAR(255)   NOT NULL,
  quantity          DECIMAL(18,4)  NOT NULL DEFAULT 1,
  unit_price_cents  DECIMAL(18,6)  NOT NULL,
  amount_cents      BIGINT         NOT NULL,
  KEY invoice_line_items_invoice (invoice_id),
  CONSTRAINT fk_invoice_line_items_invoice FOREIGN KEY (invoice_id) REFERENCES invoices (id),
  CONSTRAINT fk_invoice_line_items_unit FOREIGN KEY (unit_code) REFERENCES units (unit_code)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
