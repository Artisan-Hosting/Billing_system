// Tests for invite‑only plan handling

use billing::db::plans::is_invite_only;
use billing::grpc::admin_service::visible_to;
use serde_json::json;
use sqlx::Row;

#[test]
fn test_is_invite_only_true() {
    let meta = json!({"invite_only": true});
    assert!(is_invite_only(&meta));
}

#[test]
fn test_is_invite_only_false() {
    let meta = json!({"invite_only": false});
    assert!(!is_invite_only(&meta));
}

#[test]
fn test_is_invite_only_missing() {
    let meta = json!({});
    assert!(!is_invite_only(&meta));
}

#[test]
fn test_is_invite_only_invalid() {
    let meta = json!(123);
    assert!(!is_invite_only(&meta));
}

#[test]
fn test_visible_to_super_can_see() {
    let meta = json!({"invite_only": true});
    assert!(visible_to(&meta, true));
}

#[test]
fn test_visible_to_non_super_cannot_see() {
    let meta = json!({"invite_only": true});
    assert!(!visible_to(&meta, false));
}

#[test]
fn test_visible_to_non_invite() {
    let meta = json!({});
    assert!(visible_to(&meta, false));
    assert!(visible_to(&meta, true));
}

#[tokio::test]
#[ignore]
async fn live_db_dev_beta_is_invite_only() {
    // Requires a real database with migrations applied and DATABASE_URL set.
    let database_url = std::env::var("DATABASE_URL").expect("set DATABASE_URL");
    let pool = billing::db::connect(&database_url).await.expect("connect");

    // Fetch the raw metadata column for the dev_beta plan.
    let row = sqlx::query("SELECT metadata FROM plans WHERE plan_code = ?")
        .bind("dev_beta")
        .fetch_one(&pool)
        .await
        .expect("fetch metadata");
    let meta: serde_json::Value = row.get("metadata");
    assert!(is_invite_only(&meta));
}
