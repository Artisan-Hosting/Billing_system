-- One row per (organization_id, storefront): Developer/Business/Email/
-- Apostle-standalone are independent purchasable products per the Price
-- Book, so an org can hold one active subscription in each storefront at
-- once, not a single subscription for the whole org. `organization_id` is an
-- opaque string here, same convention domain_management's `domain_orders`
-- already uses -- no cross-database FK to ais_auth's `organizations` table.
CREATE TABLE IF NOT EXISTS subscriptions (
  id                    BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  organization_id       VARCHAR(36)  NOT NULL,
  storefront            VARCHAR(16)  NOT NULL,
  plan_code             VARCHAR(64)  NOT NULL,
  -- See billing::domain::BillingStatus: Active, PastDue, GracePeriod,
  -- Suspended, Deleted. VARCHAR, not a SQL ENUM -- same reasoning as
  -- ais_auth's rbac_policies.resource_type: a new status must never need an
  -- ALTER TABLE. Validated in Rust at the boundary instead.
  status                VARCHAR(16)  NOT NULL DEFAULT 'active',
  current_period_start  TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  current_period_end    TIMESTAMP    NOT NULL,
  -- A downgrade queued for the end of the current period (upgrades apply
  -- immediately and never populate this column).
  pending_plan_code     VARCHAR(64)  NULL,
  cancel_at_period_end  BOOLEAN      NOT NULL DEFAULT FALSE,
  created_at            TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at            TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  UNIQUE KEY uq_subscriptions_org_storefront (organization_id, storefront),
  KEY subscriptions_status (status),
  KEY subscriptions_period_end (current_period_end),
  CONSTRAINT fk_subscriptions_plan FOREIGN KEY (plan_code) REFERENCES plans (plan_code),
  CONSTRAINT fk_subscriptions_pending_plan FOREIGN KEY (pending_plan_code) REFERENCES plans (plan_code)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
