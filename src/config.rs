//! Configuration and secrets.
//!
//! Same split `domain_management` uses, for the same reason: `Config` is
//! safe to read (bind addresses, feature flags) and `Secrets` is
//! credentials only -- loaded from a `0600` env file, never logged, never
//! written to the database.
//!
//! This crate is meant to be the *one* place Stripe credentials live on
//! this platform: a service that needs to charge a customer calls
//! Billing's gRPC rather than holding its own Stripe key, so there is
//! exactly one `stripe_secret_key` in the fleet to rotate or leak.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};

pub const DEFAULT_CONFIG_PATH: &str = "/opt/artisan/etc/billing.json";
pub const DEFAULT_ENV_PATH: &str = "/opt/artisan/etc/billing.env";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub grpc: Grpc,
    pub auth: Auth,
    pub purchasing: Purchasing,
}

impl Default for Config {
    fn default() -> Self {
        Self { grpc: Grpc::default(), auth: Auth::default(), purchasing: Purchasing::default() }
    }
}

/// ais_auth's `AccountInternal` gRPC address -- `BillingAdminService`'s
/// end-user-facing RPCs validate the caller's token and check
/// `Action::Purchase` here, the same two calls `domain_management::AuthClient`
/// makes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Auth {
    /// `https://` is mutual TLS (the only thing a real ais_auth accepts);
    /// `http://` is plaintext, local dev only. Portal and domain_management
    /// default to `https://10.2.0.2:50051` for the same service; this
    /// defaults to plaintext localhost instead, matching this crate's own
    /// `Config::billing`-style "safe but must be configured for production"
    /// shape everywhere else in this file uses.
    pub grpc_addr: String,
    pub token_cache_secs: u64,
}

impl Default for Auth {
    fn default() -> Self {
        Self { grpc_addr: "http://127.0.0.1:50051".to_owned(), token_cache_secs: 60 }
    }
}

/// Kill switch for `BillingAdminService`'s two money-moving RPCs
/// (`CreateOrUpgradeSubscription`, `TopUpCredit`) -- the same shape and
/// naming as `domain_management::config::Purchasing`. Does not gate
/// `DebitCredit` (drawing down an existing balance isn't a new purchase;
/// pausing purchasing shouldn't also break already-running metering) or the
/// read-only RPCs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Purchasing {
    /// Ships `false`: nothing can be bought until someone turns it on
    /// deliberately, matching `domain_management::Purchasing::enabled`'s
    /// own default.
    pub enabled: bool,
}

impl Default for Purchasing {
    fn default() -> Self {
        Self { enabled: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grpc {
    pub bind: String,
    pub reflection: bool,
}

impl Default for Grpc {
    fn default() -> Self {
        // Not a collision with any other service's default port on this
        // platform (ais_auth, ais_secretserver, domain_management each
        // have their own) -- chosen and recorded here so it only needs
        // choosing once.
        Self { bind: "0.0.0.0:50061".to_owned(), reflection: true }
    }
}

impl Config {
    /// Missing file is not an error: the defaults above are safe to boot
    /// with, matching `domain_management::config::Config::load`'s own
    /// contract.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.unwrap_or_else(|| Path::new(DEFAULT_CONFIG_PATH));
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        let stripped = json_comments::StripComments::new(raw.as_bytes());
        serde_json::from_reader(stripped).map_err(|e| Error::Config(format!("parsing {}: {e}", path.display())))
    }
}

/// Credentials. Never logged, never stored, never returned over gRPC --
/// [`PaymentIntent.client_secret`][crate::proto::billing::PaymentIntent]
/// is the one Stripe-issued secret this service ever hands back, and only
/// in `CreatePaymentIntent`'s own response.
#[derive(Clone)]
pub struct Secrets {
    pub database_url: String,
    pub stripe_secret_key: String,
    pub stripe_webhook_secret: String,
    pub stripe_publishable_key: String,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets").finish_non_exhaustive()
    }
}

impl Secrets {
    /// Env file (default `/opt/artisan/etc/billing.env`, mode 0600) merged
    /// with process env -- env wins, same precedence
    /// `domain_management::config::Secrets::load` uses.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path.unwrap_or_else(|| Path::new(DEFAULT_ENV_PATH));
        let file_vars = parse_env_file(path).unwrap_or_default();

        let get = |key: &str| -> String {
            std::env::var(key).ok().or_else(|| file_vars.get(key).cloned()).unwrap_or_default()
        };

        Ok(Self {
            database_url: get("DATABASE_URL"),
            stripe_secret_key: get("STRIPE_SECRET_KEY"),
            stripe_webhook_secret: get("STRIPE_WEBHOOK_SECRET"),
            stripe_publishable_key: get("STRIPE_PUBLISHABLE_KEY"),
        })
    }

    /// Fails once, at boot, naming every missing credential a code path
    /// needs -- rather than a confusing error the first time that path
    /// actually runs.
    pub fn require(&self, needed: &[(&str, &str)]) -> Result<()> {
        let missing: Vec<&str> = needed.iter().filter(|(_, v)| v.is_empty()).map(|(name, _)| *name).collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(format!("missing required secret(s): {}", missing.join(", "))))
        }
    }
}

/// Tolerates `export ` prefixes and quoted values, matching the shape a
/// human hand-writes and matching `domain_management`'s own parser.
fn parse_env_file(path: &Path) -> Option<HashMap<String, String>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut vars = HashMap::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            vars.insert(key.trim().to_owned(), value.to_owned());
        }
    }

    Some(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_file_is_not_fatal() {
        let config = Config::load(Some(Path::new("/nonexistent/billing.json"))).unwrap();
        assert_eq!(config.grpc.bind, "0.0.0.0:50061");
    }

    #[test]
    fn a_missing_env_file_is_not_fatal() {
        let secrets = Secrets::load(Some(Path::new("/nonexistent/billing.env"))).unwrap();
        assert_eq!(secrets.stripe_secret_key, "");
    }

    #[test]
    fn require_names_every_missing_secret_at_once() {
        let secrets = Secrets {
            database_url: String::new(),
            stripe_secret_key: "sk_test_x".to_owned(),
            stripe_webhook_secret: String::new(),
            stripe_publishable_key: String::new(),
        };
        let err = secrets
            .require(&[
                ("DATABASE_URL", &secrets.database_url),
                ("STRIPE_SECRET_KEY", &secrets.stripe_secret_key),
                ("STRIPE_WEBHOOK_SECRET", &secrets.stripe_webhook_secret),
            ])
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("DATABASE_URL"), "{message}");
        assert!(message.contains("STRIPE_WEBHOOK_SECRET"), "{message}");
        assert!(!message.contains("STRIPE_SECRET_KEY"), "{message}");
    }

    #[test]
    fn secrets_debug_never_prints_a_value() {
        let secrets = Secrets {
            database_url: "mysql://user:hunter2@host/db".to_owned(),
            stripe_secret_key: "sk_live_super_secret".to_owned(),
            stripe_webhook_secret: "whsec_super_secret".to_owned(),
            stripe_publishable_key: "pk_live_fine_to_show".to_owned(),
        };
        let debug = format!("{secrets:?}");
        assert!(!debug.contains("hunter2"));
        assert!(!debug.contains("sk_live_super_secret"));
        assert!(!debug.contains("whsec_super_secret"));
    }
}
