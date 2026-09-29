//! The usage-cost HTTP surface (`POST /calculate`), unrelated to
//! Stripe/gRPC.
//!
//! Replaces the old hardcoded CPU/RAM/bandwidth calculator (see git history
//! for `calculate_costs` if you need it) with a plan-aware one: overage is
//! computed by `overage::calculate_pool_overage`, a pure function, against
//! whichever plan's allowances/rates are on file for the organization
//! calling this (`db::plans::allowances_and_rates_for_plan`). This route is
//! meant to be called by a monthly billing-cycle job, one call per
//! (organization, subscription), not directly by an end user.

use artisan_middleware::{
    aggregator::BilledUsageSummary,
    dusa_collection_utils::{core::logger::LogLevel, log},
    portal::{ApiResponse, ErrorCode, ErrorInfo},
};
use serde::Deserialize;
use sqlx::MySqlPool;
use warp::{Filter, http::StatusCode, reject::Rejection, reply::Reply};

use crate::db::plans;
use crate::overage::{self, OverageCharges, PoolUsage};

#[derive(Debug, Deserialize)]
struct CalculateOverageRequest {
    organization_id: String,
    plan_code: String,
    /// How many wall-clock hours `usage` covers -- needed to turn
    /// `usage.total_cpu` (core-hours) into an average vCPU count. The
    /// caller (the billing-cycle job) knows its own period length; this
    /// route has no independent way to determine it since
    /// `BilledUsageSummary` carries no period start/end.
    period_hours: f64,
    usage: BilledUsageSummary,
}

pub async fn serve_http(bind: &str, pool: MySqlPool) {
    let addr: std::net::SocketAddr = bind.parse().unwrap_or_else(|_| ([0, 0, 0, 0], 3031).into());

    let cors = warp::cors()
        .allow_methods(vec!["POST", "OPTIONS"])
        .allow_headers(vec!["content-type", "authorization"])
        .allow_any_origin();

    let pool_filter = warp::any().map(move || pool.clone());

    // POST /calculate -> ApiResponse<OverageCharges>
    let calculate = warp::path("calculate")
        .and(warp::post())
        .and(warp::body::json::<CalculateOverageRequest>())
        .and(pool_filter)
        .and_then(handle_calculate);

    // OPTIONS /calculate (preflight)
    let options = warp::path("calculate").and(warp::options()).map(warp::reply);

    let routes = calculate.or(options).recover(handle_rejection).with(cors);
    warp::serve(routes).run(addr).await;
}

async fn handle_calculate(
    req: CalculateOverageRequest,
    pool: MySqlPool,
) -> Result<impl Reply, std::convert::Infallible> {
    let catalog = match plans::allowances_and_rates_for_plan(&pool, &req.plan_code).await {
        Ok(catalog) => catalog,
        Err(err) => {
            log!(LogLevel::Warn, "overage calculation for {}: {}", req.organization_id, err);
            let resp: ApiResponse<OverageCharges> = ApiResponse {
                status: "error".to_string(),
                data: None,
                errors: vec![ErrorInfo {
                    code: ErrorCode::Whoops,
                    message: err.to_string(),
                    details: serde_json::Value::Null,
                }],
            };
            return Ok(warp::reply::with_status(warp::reply::json(&resp), StatusCode::NOT_FOUND));
        }
    };

    let pool_usage = pool_usage_from_summary(&req.usage, req.period_hours);
    let charges = overage::calculate_pool_overage(&pool_usage, &catalog.allowances, &catalog.rates);

    log!(
        LogLevel::Info,
        "overage for {} ({}): {} line item(s), ${:.2}",
        req.organization_id,
        req.plan_code,
        charges.line_items.len(),
        charges.total_cents as f64 / 100.0
    );

    let resp: ApiResponse<OverageCharges> =
        ApiResponse { status: "ok".to_string(), data: Some(charges), errors: Vec::new() };
    Ok(warp::reply::with_status(warp::reply::json(&resp), StatusCode::OK))
}

/// `avg_memory` is already an average (across samples, in MB); `total_cpu`
/// is core-hours over the whole summarized period and needs dividing by
/// `period_hours` to become an average vCPU count. Egress is a straight
/// unit conversion (bytes -> GB), since it's billed on the period total.
fn pool_usage_from_summary(summary: &BilledUsageSummary, period_hours: f64) -> PoolUsage {
    const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

    // `as f64`: on the `artisan_middleware` version currently pinned,
    // `total_cpu` is still `f32` (the fixed, `f64` core-hours version isn't
    // published/bumped into this crate's Cargo.toml yet -- see the billing
    // overhaul plan's sequencing). The cast is a no-op once it is.
    let vcpu_avg = if period_hours > 0.0 { summary.total_cpu as f64 / period_hours } else { 0.0 };

    PoolUsage {
        ram_gb_avg: summary.avg_memory / 1024.0,
        vcpu_avg,
        egress_gb_total: (summary.total_rx as f64 + summary.total_tx as f64) / BYTES_PER_GB,
        email_1k_total: 0.0, // Apostle send counting lands with the Apostle DB-backed ledger work.
    }
}

async fn handle_rejection(err: Rejection) -> Result<impl Reply, std::convert::Infallible> {
    if err.is_not_found() {
        Ok(warp::reply::with_status("NOT_FOUND", StatusCode::NOT_FOUND))
    } else {
        eprintln!("Rejection: {:?}", err);
        Ok(warp::reply::with_status("INTERNAL_SERVER_ERROR", StatusCode::INTERNAL_SERVER_ERROR))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(total_cpu: f64, avg_memory: f64, total_rx: u64, total_tx: u64) -> BilledUsageSummary {
        BilledUsageSummary {
            project_id: "org".into(),
            instance_id: "sub".into(),
            // `as f32`: see `pool_usage_from_summary`'s comment -- the
            // pinned crate version still has `total_cpu: f32`.
            total_cpu: total_cpu as f32,
            peak_cpu: 0.0,
            avg_memory,
            peak_memory: 0.0,
            total_rx,
            total_tx,
            total_samples: 1,
            instances: 1,
        }
    }

    #[test]
    fn converts_core_hours_to_an_average_vcpu_over_the_period() {
        // 365 core-hours over a 730-hour (~1 month) period = 0.5 average vCPU.
        let pool_usage = pool_usage_from_summary(&summary(365.0, 0.0, 0, 0), 730.0);
        assert_eq!(pool_usage.vcpu_avg, 0.5);
    }

    #[test]
    fn a_zero_period_never_divides_by_zero() {
        let pool_usage = pool_usage_from_summary(&summary(100.0, 0.0, 0, 0), 0.0);
        assert_eq!(pool_usage.vcpu_avg, 0.0);
    }

    #[test]
    fn converts_mb_average_to_gb_and_bytes_total_to_gb() {
        const BYTES_PER_GB: u64 = 1024 * 1024 * 1024;
        let pool_usage = pool_usage_from_summary(&summary(0.0, 2048.0, 3 * BYTES_PER_GB, 7 * BYTES_PER_GB), 730.0);
        assert_eq!(pool_usage.ram_gb_avg, 2.0);
        assert_eq!(pool_usage.egress_gb_total, 10.0);
    }
}
