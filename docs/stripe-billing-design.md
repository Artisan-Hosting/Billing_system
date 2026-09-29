# Stripe billing design (per-organization)

Status: **decided, not yet implemented.** This records the decisions made so
the implementation can proceed in the order at the bottom.

## Decisions

1. **Stripe Subscriptions own the recurring plan fee.** Stripe handles
   renewal, proration, SCA, retries/dunning emails and invoicing. The
   hand-rolled period/proration logic (`src/proration.rs`, the
   `DEFAULT_PERIOD_SECONDS` period in `CreateOrUpgradeSubscription`) is
   retired once this lands.
2. **Stripe Tax is enabled** (`automatic_tax`) on subscriptions and one-off
   charges. Configuration details are still to be researched (see
   "Stripe Tax checklist").
3. **The organization is the billing entity.** An `organization_id` exists
   before a user deploys anything, so every billing record hangs off it.
   Users never own billing data; they are granted (or denied) access to their
   organization's billing through ais_auth (`Action::Purchase` etc.).
4. **All four storefronts (developer, business, email, GPU credits) share one
   billing account per organization**: one Stripe Customer, one saved payment
   method, one credit balance.
5. **Card data never touches this platform.** Cards are collected by Stripe
   (Checkout / Payment Element / Billing Portal) and only Stripe object IDs
   are stored -- PCI SAQ-A.

## Data model additions

- `billing_customers(organization_id PK, stripe_customer_id UNIQUE,
  default_payment_method_id NULL, tax_status, created_at, updated_at)`.
  One row per organization. Created with the organization (or lazily on the
  first billing action); the Stripe Customer carries
  `metadata[organization_id]` so any webhook can be mapped back.
- `plans`: add `stripe_product_id`, `stripe_price_id`. Prices remain owned by
  the DB catalog; a `billing sync-stripe-catalog` command pushes them to
  Stripe (Prices are immutable, so a price change creates a new Price and
  moves `stripe_price_id`; existing subscribers keep theirs until migrated).
- `subscriptions`: add `stripe_subscription_id`. The row becomes a **mirror**
  of Stripe state, updated only from webhooks.
- `stripe_events(event_id PK, type, received_at, processed_at)`: webhook
  inbox for idempotency and safe replay.

## Flows

### Subscribe / upgrade / downgrade / cancel
- Subscribe: Stripe Checkout in `subscription` mode for the org's Customer
  (collects card + billing address for tax, and saves the card). Returns a
  Checkout URL instead of a PaymentIntent client secret.
- Upgrade: update the Stripe Subscription item, `proration_behavior=
  create_prorations`. Downgrade: schedule at period end.
- Cancel: `cancel_at_period_end`.
- Local `subscriptions.status` follows `customer.subscription.updated` and
  `invoice.paid` / `invoice.payment_failed` webhooks -- never optimistic.

### Overage
- Stripe emits `invoice.created` (draft, ~1h before finalization) for each
  subscription renewal. The handler fetches the org's usage for the period,
  runs the existing pure `overage::calculate_pool_overage`, and adds invoice
  items to that draft invoice. One invoice then carries plan fee + overage.
- If usage is unavailable when the hook fires, the failure must be loud
  (alert) and the invoice must not silently finalize without overage.

### GPU/LLM credits
- Local ledger stays (`credit_accounts`, `credit_ledger_entries`): per-second
  debits are far too frequent for Stripe's API.
- Top-up: Checkout `payment` mode (or PaymentIntent) against the same
  Customer, `setup_future_usage=off_session`, tax enabled.
- On `payment_intent.succeeded` / `checkout.session.completed` for a top-up,
  write a `topup` ledger entry with `idempotency_key = payment_intent id`, in
  the same transaction that records the payment status.
- `charge.refunded` and `charge.dispute.created` write negative `adjustment`
  entries.
- Auto-reload: below a per-org threshold, charge the saved payment method
  off-session; on failure alert the org and stop new GPU sessions.

### Customer self-service
- Stripe Billing Portal for card updates, invoices/receipts, tax IDs and
  cancellation.

## Webhooks to handle

`customer.subscription.{created,updated,deleted}`, `invoice.created`,
`invoice.finalized`, `invoice.paid`, `invoice.payment_failed`,
`payment_intent.{succeeded,payment_failed}`, `checkout.session.completed`,
`charge.refunded`, `charge.dispute.created`, `customer.updated` (tax/address).
Every event is recorded in `stripe_events` first; handlers are idempotent.

## Stripe Tax checklist (to research before go-live)

- Origin/head-office address and tax registrations set up in the Stripe
  dashboard for each jurisdiction where registration is required.
- Product tax codes for each Product (SaaS/hosting/email vs. professional
  services for the Care and Embed plans vs. prepaid credits).
- Customers must have an address: Checkout collects it; set
  `customer_update[address]=auto` and collect tax IDs for B2B.
- Whether prepaid GPU credits are taxed at top-up or at consumption varies by
  jurisdiction -- confirm with an accountant before choosing.
- Prices should be tax-exclusive (`tax_behavior=exclusive`) unless decided
  otherwise.

## Open questions

- One Stripe Subscription per storefront (independent lifecycles, up to four
  separate monthly invoices; current default) versus one Subscription with
  multiple items (one consolidated invoice, but a shared interval and
  lifecycle).
- When to create the Stripe Customer: at organization creation (simplest
  invariant, creates Customers for orgs that never pay) or lazily on first
  billing action.
- Grace period / suspension policy after `invoice.payment_failed` retries are
  exhausted.

## Implementation order

1. Migration for `billing_customers`, plan Price IDs, `stripe_subscription_id`,
   `stripe_events`; Stripe client gains Customer / Checkout / Portal /
   Subscription / invoice-item calls.
2. Webhook inbox + credit-topup crediting + refund/dispute handling (this is
   the launch blocker for GPU credits and stands alone).
3. Catalog sync command; Subscription create/upgrade/downgrade/cancel via
   Stripe; webhook-driven status mirroring.
4. `invoice.created` overage hook.
5. Auto-reload, Billing Portal, dunning/suspension behavior.
6. Stripe Tax enablement once the checklist above is complete.
