# Stripe billing design (per-organization)

Status: **decided, not yet implemented.** This records the decisions made so
the implementation can proceed in the order at the bottom.

## Decisions

1. **Stripe Subscriptions own the recurring plan fee.** Stripe handles
   renewal, proration, SCA, retries/dunning emails and invoicing. The
   hand-rolled period/proration logic (`src/proration.rs`, the
   `DEFAULT_PERIOD_SECONDS` period in `CreateOrUpgradeSubscription`) is
   retired once this lands.
2. **Stripe Tax is NOT used.** The services are classed as data processing and
   tax is handled through the company's existing process, outside Stripe. No
   `automatic_tax`, no tax codes, no address collection *for tax purposes*.
   (If that classification ever changes, Stripe Tax can be switched on later
   per subscription without a redesign.)
3. **The organization is the billing entity.** An `organization_id` exists
   before a user deploys anything, so every billing record hangs off it.
   Users never own billing data; they are granted (or denied) access to their
   organization's billing through ais_auth (`Action::Purchase` etc.).
4. **All four storefronts (developer, business, email, GPU credits) share one
   billing account per organization**: one Stripe Customer, one saved payment
   method, one credit balance.
5. **One Stripe Subscription per storefront** (matches the existing
   `(organization_id, storefront)` uniqueness). Independent lifecycles; an org
   on several storefronts gets one invoice per storefront per month.
6. **The Stripe Customer is created when the organization is created**, so
   every org always has a billing account. This also allows issuing invoices
   with no card on file -- notably **$0.00 invoices for the invite-only Beta
   program** (`dev_beta` is a $0 plan). A $0 Subscription needs no payment
   method and produces a $0.00 invoice/receipt each period; nothing is
   charged.
7. **Card data never touches this platform.** Cards are collected by Stripe
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

## Pricing model findings (from Artisan_Pricing_Model.xlsx)

The spreadsheet is the source for prices, allowances and policy; migration
`0003_plan_catalog_seed.sql` mirrors it. Things the billing implementation
has to respect or the spreadsheet does not yet cover:

- **Stripe fee is modeled as 2.9% + $0.30 only.** Stripe Billing (Subscriptions/
  Invoicing) carries an additional per-volume fee (roughly 0.7% at the time of
  writing -- confirm current pricing). Add it to `Inputs` so the margins on
  the small plans (Builder $8, Mail Starter $12) are honest. $0 invoices
  (Beta) incur no percentage fee.
- **GPU credits are integer cents, but GPU rates are far below a cent per
  second.** A T4 at $0.30/hr is 0.0083 cents/second, and `DebitCredit` is
  specified as roughly one debit per second per session with a positive
  integer `amount_cents`. Rounding each tick up over- or under-charges badly.
  Fix before GPU launch: either meter in a finer unit (e.g. micro-dollars in
  `balance_*` / `amount_*`), or have RunpodManager accumulate the fractional
  remainder and debit in whole cents (carrying the remainder), or debit per
  minute rather than per second. Recommendation: keep the ledger in a finer
  unit -- it keeps `DebitCredit` idempotent and lossless.
- **GPU margin is small in absolute terms** (10 h/mo sold = $1.60). The $25
  minimum top-up already keeps the Stripe fixed fee (~4%) well below the
  ~26% gross margin; keep that floor.
- **Catalog gaps between the spreadsheet and the DB seed:** the sheet defines
  the paused VM-S/M/L plans and a Managed VM add-on ($25) that are not in
  `plans`; and the Care plans, Business/Managed mailboxes and Apostle
  allowances (`biz_care` 3 mailboxes, `biz_managed` 10 mailboxes,
  `apostle_dedicated` 2 GB RAM / 1 vCPU, care-plan `email_1k`) exist in the
  sheet but not in `plan_allowances`. Decide which are real and reconcile.
- **Apostle overage** ($0.80/1k, $0.40/1k dedicated) exists in the DB seed
  but not in the spreadsheet `Inputs`; add it so the sheet and DB agree.
- **Care/Embed plans** (`biz_*`, `embed_*`) are flat monthly Stripe prices;
  Embed hours are tracked as allowances but never billed as overage.

## Open questions

- Grace period / suspension policy after `invoice.payment_failed` retries are
  exhausted (maps to the existing `PastDue -> GracePeriod -> Suspended ->
  Deleted` states).
- Credit unit (see the GPU finding above): micro-dollars vs. carrying
  remainders in RunpodManager.
- Whether credits expire, are refundable, or can go negative (currently
  `DebitCredit` may leave a negative balance by design).
- How an admin issues a manual $0 (or comped) invoice for Beta orgs: Stripe
  dashboard, or a new `BillingAdminService` RPC.

## Implementation order

1. Create the Stripe Customer at organization creation (hook or RPC called by
   whatever creates the org), plus a migration for `billing_customers`, plan Price IDs, `stripe_subscription_id`,
   `stripe_events`; Stripe client gains Customer / Checkout / Portal /
   Subscription / invoice-item calls.
2. Webhook inbox + credit-topup crediting + refund/dispute handling (this is
   the launch blocker for GPU credits and stands alone).
3. Catalog sync command; Subscription create/upgrade/downgrade/cancel via
   Stripe; webhook-driven status mirroring.
4. `invoice.created` overage hook.
5. Auto-reload, Billing Portal, dunning/suspension behavior.
