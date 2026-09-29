-- Seeds the approved Price Book v1 plans (see
-- Platform/docs/pricing/Artisan_Pricing_Model.xlsx and the Price Book doc).
-- RAM/vCPU figures are the olympus-host-adjusted numbers (4 physical cores,
-- 16 sellable vCPU is the binding constraint on that host, not RAM) --
-- Darrion confirmed these 2026-09-27. A price change belongs here as a data
-- migration, or as an UPDATE against these tables in a later one -- never
-- back in application code.
--
-- Care plans (Essentials/Business/Managed Platform) get an allowance row for
-- margin/capacity reference only -- no overage rate, since they are managed
-- services sold on engineer time, not metered pooled infra like the
-- Developer plans are.

INSERT INTO plans (plan_code, storefront, display_name, price_cents, metadata) VALUES
  ('dev_beta',    'developer', 'Beta',    0,     JSON_OBJECT('invite_only', TRUE, 'capped_at', 'dev_builder')),
  ('dev_builder', 'developer', 'Builder', 800,   NULL),
  ('dev_pro',     'developer', 'Pro',     3200,  NULL),
  ('dev_team',    'developer', 'Team',    9500,  NULL),
  ('biz_essentials', 'business', 'Essentials Care',   3000,  NULL),
  ('biz_care',       'business', 'Business Care',     9900,  NULL),
  ('biz_managed',    'business', 'Managed Platform',  30000, NULL),
  ('embed_10', 'business', 'Embed 10hr/mo', 110000, NULL),
  ('embed_20', 'business', 'Embed 20hr/mo', 200000, NULL),
  ('embed_40', 'business', 'Embed 40hr/mo', 380000, NULL),
  ('mail_starter',  'email', 'Mail Starter',  1200, NULL),
  ('mail_business', 'email', 'Mail Business', 2500, NULL),
  ('mail_team',     'email', 'Mail Team',     4500, NULL),
  ('apostle_send_10k',  'email', 'Send 10k',  1000, NULL),
  ('apostle_send_50k',  'email', 'Send 50k',  3000, NULL),
  ('apostle_send_150k', 'email', 'Send 150k', 7500, NULL),
  ('apostle_dedicated', 'email', 'Dedicated Apostle', 15000, NULL)
ON DUPLICATE KEY UPDATE display_name = VALUES(display_name), price_cents = VALUES(price_cents);

-- Developer storefront: RAM + vCPU pool, plus egress and included Apostle
-- sending. Beta mirrors Builder's allowance exactly (invite-only, capped at
-- Builder per the Price Book).
INSERT INTO plan_allowances (plan_code, unit_code, included_qty) VALUES
  ('dev_beta',    'ram_gb_month', 0.5),
  ('dev_beta',    'vcpu_month',   0.25),
  ('dev_beta',    'egress_gb',    10),
  ('dev_beta',    'email_1k',     1),
  ('dev_builder', 'ram_gb_month', 0.5),
  ('dev_builder', 'vcpu_month',   0.25),
  ('dev_builder', 'egress_gb',    10),
  ('dev_builder', 'email_1k',     1),
  ('dev_pro',     'ram_gb_month', 2),
  ('dev_pro',     'vcpu_month',   1),
  ('dev_pro',     'egress_gb',    50),
  ('dev_pro',     'email_1k',     5),
  ('dev_team',    'ram_gb_month', 6),
  ('dev_team',    'vcpu_month',   3),
  ('dev_team',    'egress_gb',    200),
  ('dev_team',    'email_1k',     25),
  ('biz_essentials', 'ram_gb_month', 0.25),
  ('biz_essentials', 'vcpu_month',   0.25),
  ('biz_care',       'ram_gb_month', 1),
  ('biz_care',       'vcpu_month',   1),
  ('biz_managed',    'ram_gb_month', 4),
  ('biz_managed',    'vcpu_month',   2),
  ('embed_10', 'engineer_hour', 10),
  ('embed_20', 'engineer_hour', 20),
  ('embed_40', 'engineer_hour', 40),
  ('mail_starter',  'mailbox', 3),
  ('mail_business', 'mailbox', 10),
  ('mail_team',     'mailbox', 25),
  ('apostle_send_10k',  'email_1k', 10),
  ('apostle_send_50k',  'email_1k', 50),
  ('apostle_send_150k', 'email_1k', 150),
  ('apostle_dedicated', 'email_1k', 500)
ON DUPLICATE KEY UPDATE included_qty = VALUES(included_qty);

-- Overage: Developer plans only (Care plans are not metered past their
-- allowance -- they're managed services, not self-serve pooled infra).
-- Rates confirmed 2026-09-27 against the real olympus host specs.
INSERT INTO plan_overage_rates (plan_code, unit_code, rate_cents_per_unit) VALUES
  ('dev_beta',    'ram_gb_month', 1000),
  ('dev_beta',    'vcpu_month',   1300),
  ('dev_beta',    'egress_gb',    5),
  ('dev_builder', 'ram_gb_month', 1000),
  ('dev_builder', 'vcpu_month',   1300),
  ('dev_builder', 'egress_gb',    5),
  ('dev_pro',     'ram_gb_month', 1000),
  ('dev_pro',     'vcpu_month',   1300),
  ('dev_pro',     'egress_gb',    5),
  ('dev_team',    'ram_gb_month', 1000),
  ('dev_team',    'vcpu_month',   1300),
  ('dev_team',    'egress_gb',    5),
  ('apostle_send_10k',  'email_1k', 80),
  ('apostle_send_50k',  'email_1k', 80),
  ('apostle_send_150k', 'email_1k', 80),
  ('apostle_dedicated', 'email_1k', 40)
ON DUPLICATE KEY UPDATE rate_cents_per_unit = VALUES(rate_cents_per_unit);
