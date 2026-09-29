-- The shared unit catalog and plan catalog (Price Book v1). Plan data is DB
-- rows rather than hardcoded constants, so a price change is a data
-- migration, not a redeploy -- see src/usage.rs's old calculate_costs for
-- what hardcoding this looked like.

-- One row per unit in the shared catalog every storefront sells from: a GB
-- of RAM for a month, a vCPU for a month, a GB of egress, a GPU-hour, a
-- domain, a mailbox, 1,000 Apostle-sent emails, an hour of engineer time.
CREATE TABLE IF NOT EXISTS units (
  unit_code   VARCHAR(32) NOT NULL PRIMARY KEY,
  description VARCHAR(255) NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT INTO units (unit_code, description) VALUES
  ('ram_gb_month',   'One GB of RAM, pooled and billed for one month'),
  ('vcpu_month',     'One vCPU, pooled and billed for one month'),
  ('egress_gb',      'One GB of outbound network traffic'),
  ('gpu_hour',       'One GPU-hour of prepaid Runpod compute'),
  ('domain',         'One registered domain'),
  ('mailbox',        'One resold mailbox (Artisan Mail / MXroute)'),
  ('email_1k',       'One thousand Apostle-sent emails'),
  ('engineer_hour',  'One hour of engineer time (Embed retainers)')
ON DUPLICATE KEY UPDATE description = VALUES(description);

-- One row per sellable plan across both storefronts. `metadata` carries
-- shape that doesn't need its own column to query on (e.g. a hard RAM cap
-- enforced elsewhere via `max_ram_usage`, never billed) -- see the Price
-- Book doc's Developer/Business storefront tables for what each plan means.
CREATE TABLE IF NOT EXISTS plans (
  plan_code        VARCHAR(64)  NOT NULL PRIMARY KEY,
  -- 'developer' | 'business' | 'email' | 'gpu' -- which storefront sells
  -- this plan. Not a SQL ENUM, for the same reason rbac_policies.resource_type
  -- isn't one (see ais_auth's 0005_rbac_policies.sql): a new storefront
  -- should never need an ALTER TABLE.
  storefront       VARCHAR(16)  NOT NULL,
  display_name     VARCHAR(128) NOT NULL,
  price_cents      BIGINT       NOT NULL,
  currency         CHAR(3)      NOT NULL DEFAULT 'usd',
  billing_interval VARCHAR(16)  NOT NULL DEFAULT 'monthly',
  active           BOOLEAN      NOT NULL DEFAULT TRUE,
  metadata         JSON         NULL,
  created_at       TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at       TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  KEY plans_storefront (storefront, active)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- What's included in a plan's monthly price, per unit -- the pool a customer
-- draws from before overage applies.
CREATE TABLE IF NOT EXISTS plan_allowances (
  plan_code    VARCHAR(64)   NOT NULL,
  unit_code    VARCHAR(32)   NOT NULL,
  included_qty DECIMAL(18,4) NOT NULL,
  PRIMARY KEY (plan_code, unit_code),
  CONSTRAINT fk_plan_allowances_plan FOREIGN KEY (plan_code) REFERENCES plans (plan_code),
  CONSTRAINT fk_plan_allowances_unit FOREIGN KEY (unit_code) REFERENCES units (unit_code)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- What a unit costs per-unit once a plan's allowance is exceeded, billed on
-- the monthly average (never peak -- see billing::overage::calculate_pool_overage).
CREATE TABLE IF NOT EXISTS plan_overage_rates (
  plan_code           VARCHAR(64)    NOT NULL,
  unit_code           VARCHAR(32)    NOT NULL,
  rate_cents_per_unit DECIMAL(18,6)  NOT NULL,
  PRIMARY KEY (plan_code, unit_code),
  CONSTRAINT fk_plan_overage_rates_plan FOREIGN KEY (plan_code) REFERENCES plans (plan_code),
  CONSTRAINT fk_plan_overage_rates_unit FOREIGN KEY (unit_code) REFERENCES units (unit_code)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
