# Billing

Internal gRPC gateway to Stripe. It is the *one* place Stripe credentials live
on this platform, and it owns plans, subscriptions, invoices and prepaid credit
(`BillingService` / `BillingAdminService`, see `proto/billing.proto`). Any
service that needs to charge a customer calls this crate's gRPC rather than
holding its own Stripe key. Metered overage is priced by
`BillingAdminService.RecordOverageUsage`. There is no HTTP surface.

## Configuration

Configuration is split the same way `domain_management` splits it:

- `Config` (`billing.json`) -- safe to read: bind addresses, the ais_auth
  gRPC address, feature flags. A missing file is not an error; every field
  falls back to a safe default (see `src/config.rs`).
- `Secrets` (`billing.env`) -- credentials only, loaded from a `0600` file
  and never logged. Process environment variables of the same name take
  precedence over the file.

Example copies of both live in [`examples/`](examples/):

```
cp examples/billing.json /opt/artisan/etc/billing.json
cp examples/billing.env  /opt/artisan/etc/billing.env
chmod 0600 /opt/artisan/etc/billing.env
```

Edit them, then fill in real Stripe keys and a `DATABASE_URL` (MySQL) in the
env file. Both paths can also be overridden on the command line:

```
billing --config /path/to/billing.json --env-file /path/to/billing.env
```

## Building and running

A `Makefile` wraps the common commands (defaulting to the example config so
`make run` works out of the box for local development):

```
make build     # cargo build
make run       # run the server against examples/billing.json + billing.env
make migrate   # prints a reminder: migrations are applied by hand
make test      # cargo test
make check     # fmt-check + clippy + test
```

Migrations live in `migrations/` and are applied by hand (`serve` and
`billing migrate` do not run them); `0008_invoices_subscription_status_idx.sql`
adds the `(subscription_id, status)` index for the paid-invoice lookup and, like
every migration, is applied by hand.
