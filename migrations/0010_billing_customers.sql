-- One Stripe Customer per organization. The organization is the billing
-- entity for all four storefronts (developer, business, email, GPU credits);
-- users never own billing data. The Customer is created when the
-- organization is created (see `EnsureCustomer` in proto/billing.proto) so an
-- org can always be invoiced -- including a $0.00 invoice for a Beta org that
-- has no card on file.
CREATE TABLE IF NOT EXISTS billing_customers (
  organization_id           VARCHAR(36)  NOT NULL PRIMARY KEY,
  stripe_customer_id        VARCHAR(128) NOT NULL,
  -- A Stripe PaymentMethod id (pm_...) saved off-session from a completed
  -- payment; what auto-reload and future off-session charges use. Only the
  -- id is stored here -- card data never touches this platform.
  default_payment_method_id VARCHAR(128) NULL,
  created_at                TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at                TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  UNIQUE KEY billing_customers_stripe_id (stripe_customer_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
