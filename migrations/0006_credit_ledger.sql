-- GPU/LLM prepaid credit balance. `credit_accounts.balance_cents` is a
-- materialized column kept in sync with `credit_ledger_entries` inside the
-- same transaction as every write -- balance reads are the hot path (a
-- pre-flight check on every GPU session start), so this avoids a SUM() over
-- the whole ledger on every check. The ledger itself is append-only and
-- stays the source of truth for audit/reconciliation.
CREATE TABLE IF NOT EXISTS credit_accounts (
  organization_id         VARCHAR(36) NOT NULL PRIMARY KEY,
  balance_cents           BIGINT      NOT NULL DEFAULT 0,
  monthly_spend_cap_cents BIGINT      NULL,
  -- Debounces the 20%-balance alert so it fires once per low-balance episode,
  -- not on every debit while the balance stays low.
  low_balance_alerted_at  TIMESTAMP   NULL,
  updated_at              TIMESTAMP   NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS credit_ledger_entries (
  id                  BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  organization_id     VARCHAR(36)  NOT NULL,
  -- topup | debit | adjustment
  entry_type          VARCHAR(16)  NOT NULL,
  -- Signed: a topup is positive, a debit is negative, so summing this column
  -- for an organization always reconciles to `credit_accounts.balance_cents`.
  amount_cents        BIGINT       NOT NULL,
  -- The Runpod session id a debit metered, or the Stripe payment_intent id a
  -- topup was funded by -- opaque to this table, same convention
  -- payment_intents.external_reference already uses.
  external_reference  VARCHAR(191) NULL,
  balance_after_cents BIGINT       NOT NULL,
  created_at          TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY credit_ledger_entries_org (organization_id, created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
