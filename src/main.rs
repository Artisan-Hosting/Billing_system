use artisan_middleware::{
    aggregator::{BilledUsageSummary, BillingCosts}, dusa_collection_utils::{log, core::logger::{set_log_level, LogLevel}}, portal::{ApiResponse, BillingParams}
};
use warp::{http::StatusCode, reject::Rejection, reply::Reply, Filter};

#[tokio::main]
async fn main() {
    set_log_level(LogLevel::Trace);

    let cors = warp::cors()
        .allow_methods(vec!["POST", "OPTIONS"])
        .allow_headers(vec![
            "content-type",
            "authorization",
        ])
        .allow_any_origin();
    // .allow_origin("http://localhost:3000")
    // .allow_origin("https://dashboard.artisanhosting.net");

    // POST /calculate → ApiResponse<BillingCosts>
    let calculate = warp::path("calculate")
        .and(warp::post())
        .and(warp::query::<BillingParams>())
        .and(warp::body::json::<BilledUsageSummary>())
        .map(|params: BillingParams, usage: BilledUsageSummary | {

            // do the math
            let costs: BillingCosts = calculate_costs(&usage, params.instances);
            log!(LogLevel::Info, "\nBill generated for {}: \n{}", usage.runner_id, costs);

            // wrap in your ApiResponse
            let resp: ApiResponse<BillingCosts> = ApiResponse {
                status: "ok".to_string(),
                data: Some(costs),
                errors: Vec::new(),
            };


            warp::reply::json(&resp)
        });

    // 3) OPTIONS /calculate (preflight)
    let options = warp::path("calculate")
        .and(warp::options())
        .map(warp::reply);

    // 4) Combine and apply CORS
    let routes = calculate.or(options).recover(handle_rejection).with(cors) ;
    warp::serve(routes).run(([0, 0, 0, 0], 3031)).await;
}

/// Calculate billing costs from a `UsageSummary`.
///
/// - `instance_count` lets you split a grouped summary evenly if you wish;
///   default it to `1` when billing the entire group as a whole.
/// - Applies a $15 minimum fee if the computed total is below that.
pub fn calculate_costs(summary: &BilledUsageSummary, instance_count: u64) -> BillingCosts {
    // CPU pricing
    let vcpu_hours = summary.total_cpu as f64; // Already core-hours now
    let cpu_cost = vcpu_hours * 2.00; // $2.00 per vCPU hour

    // RAM pricing
    let ram_mb: f64 = summary.avg_memory.into();
    let ram_peak_4th = (summary.peak_memory.round() as f64) / 4.0; // peak/4
    let ram_total_mb = ram_mb + ram_peak_4th;
    let ram_total_gb = ram_total_mb / 1024.0;
    let ram_cost = ram_total_gb * 3.00; // $2.00 per GB (adjust if you want $/month)

    // Bandwidth pricing
    let tx_gb = summary.total_tx as f64 / (1024.0 * 1024.0 * 1024.0);
    let rx_gb = summary.total_rx as f64 / (1024.0 * 1024.0 * 1024.0);
    let total_gress = (tx_gb / 2.0) + rx_gb;
    let bandwidth_cost = if total_gress <= 5.0 {
        0.0
    } else if total_gress <= 50.0 {
        (total_gress - 5.0) * 1.25  // discounted base rate
    } else if total_gress <= 100.0 {
        (45.0 * 1.25) + (total_gress - 50.0) * 1.00
    } else {
        (45.0 * 1.25) + (50.0 * 1.00) + (total_gress - 100.0) * 0.75
    };
    
    let mut total_cost = cpu_cost + ram_cost + bandwidth_cost;
    total_cost += (instance_count * 5) as f64;


    BillingCosts {
        cpu_cost,
        ram_cost,
        bandwidth_cost,
        total_cost,
        instances: instance_count as u64
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
    use artisan_middleware::aggregator::BilledUsageSummary;

    #[test]
    fn test_calculate_costs_minimum() {
        let usage = BilledUsageSummary {
            runner_id: "r".into(),
            instance_id: "i".into(),
            total_cpu: 0.0,
            peak_cpu: 0.0,
            avg_memory: 0.0,
            peak_memory: 0.0,
            total_rx: 0,
            total_tx: 0,
            total_samples: 1,
            instances: 1,
        };
        let costs = calculate_costs(&usage, 1);
        assert_eq!(costs.total_cost, 5.0);
    }

    #[test]
    fn test_calculate_costs_scaled() {
        let usage = BilledUsageSummary {
            runner_id: "r".into(),
            instance_id: "i".into(),
            total_cpu: 3600.0, // means ~100% CPU for 1 hour
            peak_cpu: 100.0,
            avg_memory: 1024.0,        // 1 GB avg
            peak_memory: 2048.0,       // 2 GB peak
            total_rx: 1024 * 1024,     // 1 MB
            total_tx: 2 * 1024 * 1024, // 2 MB
            total_samples: 3600,
            instances: 1,       // 1 hour worth
        };
        let costs = calculate_costs(&usage, 1);
        // CPU: 1 vcpu-hour * $2 = $2
        assert!((costs.cpu_cost - 2.0).abs() < 1e-6);
        // RAM: (1024 + 2048/4) MB * 0.001953125 ≈ (1024+512)*.001953125 = 3 MB * .001953125 = ~3,
        //   actually 1536*.001953125 = 3.0
        assert!((costs.ram_cost - 3.0).abs() < 1e-6);
        // Bandwidth: 3 MB total * $0.005 = $0.015
        assert!((costs.bandwidth_cost - 0.015).abs() < 1e-6);
        // Total: 2 + 3 + 0.015 = 5.015 → minimum is $15, so expect 15
        assert_eq!(costs.total_cost, 5.0);
    }
}
