//! One error type for the service, plus the mapping to gRPC statuses.
//!
//! Mirrors `domain_management/src/error.rs`'s shape and rationale: the
//! variant chosen here is what a caller (an internal service, not a human)
//! gets back as a `tonic::Status` code, so it needs to be right.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),

    #[error("database: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("stripe: {0}")]
    Stripe(String),

    #[error("{0} not found")]
    NotFound(String),

    #[error("not permitted: {0}")]
    Forbidden(String),

    #[error("invalid request: {0}")]
    Invalid(String),

    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    /// A service this one depends on (ais_auth) could not be reached. Kept
    /// distinct from `Invalid` so a caller can tell "the policy engine is
    /// down, retry" from "you sent nonsense" -- a denial and an outage must
    /// never look the same. Same reasoning `domain_management::Error::Unavailable`
    /// exists for.
    #[error("unavailable: {0}")]
    Unavailable(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for tonic::Status {
    fn from(err: Error) -> Self {
        match err {
            Error::Unauthenticated(msg) => tonic::Status::unauthenticated(msg),
            Error::Unavailable(msg) => tonic::Status::unavailable(msg),
            Error::Forbidden(msg) => tonic::Status::permission_denied(msg),
            Error::Invalid(msg) => tonic::Status::invalid_argument(msg),
            Error::NotFound(what) => tonic::Status::not_found(format!("{what} not found")),
            // Everything else is ours to fix, not the caller's. The detail
            // stays in the message because every caller is internal.
            other => tonic::Status::internal(other.to_string()),
        }
    }
}
