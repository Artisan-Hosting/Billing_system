//! Talking to `ais_auth`.
//!
//! Trimmed to exactly what `BillingAdminService` needs: validating a
//! caller's access token, and asking the RBAC policy engine whether
//! `Action::Purchase` on `ResourceType::Subscription` is allowed. See
//! `domain_management::auth::AuthClient` for the fuller client this mirrors
//! -- Billing has no CLI, no runner adoption flow, and mints no tokens of
//! its own, so `login`/`elevate`/`list_organizations`/`assign_runner_org`
//! have no counterpart here.

use artisan_middleware::api::claims::Claims;
use tonic::transport::Channel;

use crate::error::{Error, Result};
use crate::proto::accounts::account_internal_client::AccountInternalClient;
use crate::proto::accounts::{EvaluateAccessRequest, TokenRequest};

#[derive(Clone)]
pub struct AuthClient {
    channel: Channel,
}

impl AuthClient {
    /// Lazy connection -- the same pattern `domain_management::AuthClient::new`
    /// uses, and for the same reason: constructing a client that never gets
    /// called (this crate's own `BillingService` never needs ais_auth) costs
    /// nothing.
    ///
    /// The address scheme picks the transport. `https://` presents this
    /// service's own client certificate (`MTLS_CERT_PATH`/`MTLS_KEY_PATH`/
    /// `MTLS_CA_PATH`, defaulting to `/etc/artisan/tls/ais_billing.{crt,key}`
    /// and `ca.crt`) and checks ais_auth's certificate against the name
    /// `ais_auth` (override with `AUTH_TLS_SERVER_NAME`). `http://` is
    /// plaintext and won't reach a real ais_auth.
    ///
    /// Must be called from inside a Tokio runtime (`connect_lazy` registers
    /// with the reactor and panics outside one).
    pub fn new(addr: &str) -> Result<Self> {
        let config_err = |detail: String| Error::Config(format!("auth.grpc_addr {addr:?}: {detail}"));

        let mtls = if crate::mtls_client::wants_tls(addr) {
            Some(crate::mtls_client::ClientMtls::load("ais_billing").map_err(config_err)?)
        } else {
            None
        };
        let server_name = std::env::var("AUTH_TLS_SERVER_NAME").unwrap_or_else(|_| "ais_auth".to_owned());
        let channel =
            crate::mtls_client::internal_channel(addr, &server_name, mtls.as_ref()).map_err(config_err)?;

        Ok(Self { channel })
    }

    fn client(&self) -> AccountInternalClient<Channel> {
        AccountInternalClient::new(self.channel.clone())
    }

    /// Validates an access token and returns who it belongs to. Claims come
    /// back as a string map and are rebuilt with `Claims::from_map`, the
    /// same type Portal/domain_management/ais_auth pass around, so a role
    /// or org comparison means the same thing everywhere.
    pub async fn validate(&self, access_token: &str) -> Result<Claims> {
        let response = self
            .client()
            .validate_token(TokenRequest { access_token: access_token.to_owned() })
            .await
            .map_err(status_to_error)?
            .into_inner();

        if !response.valid {
            return Err(Error::Unauthenticated("token rejected by ais_auth".to_owned()));
        }

        Claims::from_map(response.claims).map_err(|e| Error::Unauthenticated(format!("claims from ais_auth: {e}")))
    }

    /// The generic RBAC decision: may this caller perform `action` on this
    /// resource? Asked of ais_auth rather than worked out here, the same
    /// reasoning `domain_management::AuthClient::evaluate_access` documents.
    ///
    /// `resource_type` is a plain string here, not the typed
    /// `artisan_middleware::identity::ResourceType` enum
    /// `domain_management`'s own client uses -- the RPC wire format is a
    /// string either way (`ResourceType::as_str()` on the sending side,
    /// `ResourceType::from_str()` on ais_auth's), and Billing currently
    /// pins an `artisan_middleware` version that predates
    /// `ResourceType::Subscription` (added alongside this billing overhaul,
    /// not yet published -- see the billing overhaul plan's sequencing).
    /// Taking a string avoids coupling this crate's compile to that
    /// publish; callers pass `"subscription"` today. Once the crate is
    /// published and this pin is bumped, this can switch back to the typed
    /// enum without changing the wire behavior at all.
    ///
    /// A failure to reach ais_auth is an error, never `false`: the caller
    /// turns it into `Unavailable` so a denial can never be confused with
    /// the policy engine being down.
    pub async fn evaluate_access(
        &self,
        claims: &Claims,
        resource_type: &str,
        resource_id: &str,
        action: artisan_middleware::identity::Action,
    ) -> Result<bool> {
        let response = self
            .client()
            .evaluate_access(EvaluateAccessRequest {
                claims: claims.to_map(),
                resource_type: resource_type.to_owned(),
                resource_id: resource_id.to_owned(),
                action: action.as_str().to_owned(),
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        Ok(response.yes)
    }
}

fn status_to_error(status: tonic::Status) -> Error {
    match status.code() {
        tonic::Code::Unauthenticated => Error::Unauthenticated(status.message().to_owned()),
        tonic::Code::PermissionDenied => Error::Forbidden(status.message().to_owned()),
        tonic::Code::InvalidArgument => Error::Invalid(status.message().to_owned()),
        tonic::Code::Unavailable => Error::Unavailable(format!("ais_auth is unreachable: {}", status.message())),
        _ => Error::Invalid(format!("ais_auth: {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_bad_address_fails_at_construction_not_at_first_use() {
        assert!(AuthClient::new("not a url").is_err());
        // Valid address, no server: lazy connect means this still succeeds.
        assert!(AuthClient::new("http://127.0.0.1:50051").is_ok());
    }

    #[test]
    fn unauthenticated_statuses_keep_their_meaning() {
        let error = status_to_error(tonic::Status::unauthenticated("bad token"));
        assert!(matches!(error, Error::Unauthenticated(_)));

        let error = status_to_error(tonic::Status::permission_denied("not an admin"));
        assert!(matches!(error, Error::Forbidden(_)));
    }
}
