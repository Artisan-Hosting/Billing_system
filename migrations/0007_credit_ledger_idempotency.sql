-- `DebitCreditRequest.idempotency_key` (proto/billing.proto) needs somewhere
-- to live: a retried debit (RunpodManager's metering loop retrying after a
-- timeout) must land once, not twice, the same reasoning
-- `payment_intents.stripe_payment_intent_id` is UNIQUE. NULL is allowed for
-- entries that predate this column or never had one (a manual adjustment);
-- the uniqueness constraint only applies to non-NULL values, MySQL's normal
-- behavior for a UNIQUE key with NULLs.
ALTER TABLE credit_ledger_entries
  ADD COLUMN idempotency_key VARCHAR(191) NULL AFTER external_reference,
  ADD UNIQUE KEY uq_credit_ledger_entries_org_idempotency (organization_id, idempotency_key);
