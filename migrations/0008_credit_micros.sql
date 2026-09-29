-- The credit ledger moves from cents to micro-dollars (1 USD = 1,000,000;
-- 1 cent = 10,000). GPU rates are far below a cent per second (a $0.30/hr T4
-- is ~0.0083 cents/s) and RunpodManager meters about once a second, so whole
-- cents cannot represent a single tick. See src/credit.rs.
--
-- Existing rows are converted in place (x 10,000), so balances and the
-- append-only ledger keep reconciling: SUM(amount_micros) still equals
-- credit_accounts.balance_micros.
ALTER TABLE credit_accounts
  CHANGE COLUMN balance_cents balance_micros BIGINT NOT NULL DEFAULT 0,
  CHANGE COLUMN monthly_spend_cap_cents monthly_spend_cap_micros BIGINT NULL;

UPDATE credit_accounts
   SET balance_micros = balance_micros * 10000,
       monthly_spend_cap_micros = monthly_spend_cap_micros * 10000;

ALTER TABLE credit_ledger_entries
  CHANGE COLUMN amount_cents amount_micros BIGINT NOT NULL,
  CHANGE COLUMN balance_after_cents balance_after_micros BIGINT NOT NULL;

UPDATE credit_ledger_entries
   SET amount_micros = amount_micros * 10000,
       balance_after_micros = balance_after_micros * 10000;
