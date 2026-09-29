use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/billing.proto");

    // The descriptor set feeds tonic-reflection, matching domain_management,
    // ais_auth, and ais_secretserver's own build.rs -- `grpcurl -plaintext
    // <addr> list` works against a running Billing the same way.
    let descriptor_path = PathBuf::from(env::var("OUT_DIR")?).join("billing_descriptor.bin");

    tonic_prost_build::configure()
        .build_server(true)
        // The client is built too: any internal caller (domain_management,
        // and whatever other system leverages this next) vendors this same
        // proto to reach Billing, the same arrangement Portal has with
        // domain_management's own `domains.proto`.
        .build_client(true)
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&["proto/billing.proto"], &["proto"])?;

    // === accounts.proto, vendored from ais_auth ===
    //
    // BillingAdminService's end-user-facing RPCs need to validate a caller's
    // access token and ask ais_auth's RBAC policy engine whether
    // Action::Purchase is allowed, the same two calls
    // domain_management::AuthClient makes. Client-only: Billing never serves
    // AccountInternal itself. Same vendoring arrangement as billing.proto has
    // in domain_management's own build.rs -- does NOT auto-sync, copy the
    // file by hand from ais_proto/accounts.proto after editing it there.
    println!("cargo:rerun-if-changed=proto/accounts.proto");

    tonic_prost_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/accounts.proto"], &["proto"])?;

    Ok(())
}
