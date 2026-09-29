//! The generated `billing` protobuf module.
//!
//! Vendored by whatever internal service calls this one -- the wire format
//! is the only thing that has to agree between crates, the same
//! arrangement `domain_management`'s `domains.proto` already has with
//! Portal's own older-tonic copy.

pub mod billing {
    tonic::include_proto!("billing");

    /// Fed to tonic-reflection so `grpcurl -plaintext <addr> list` works.
    pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("billing_descriptor");
}

/// ais_auth's generated gRPC client, vendored the same way
/// `domain_management` vendors it -- used only by `auth`.
pub mod accounts {
    tonic::include_proto!("accounts");
}
