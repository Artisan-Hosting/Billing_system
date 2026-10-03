-- Index for the paid-invoice lookup in subscriptions_with_paid_invoice:
-- SELECT DISTINCT subscription_id FROM invoices WHERE status = 'paid' AND subscription_id IN (...)
-- Enables an index-only scan on (subscription_id, status).
CREATE INDEX IF NOT EXISTS invoices_subscription_status ON invoices (subscription_id, status);
