//! The `health` subcommand: asks a running server, through `grpc.health.v1`,
//! whether it serves. For container health checks, where no `grpcurl` or
//! `grpc_health_probe` is installed.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tonic::transport::Endpoint;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

/// Exits successfully when `service` on `endpoint` answers `SERVING` within
/// `timeout`.
pub(crate) async fn check(endpoint: &str, service: &str, timeout: Duration) -> Result<()> {
    let channel = Endpoint::from_shared(endpoint.to_owned())
        .with_context(|| format!("`{endpoint}` is not a valid endpoint"))?
        .connect_timeout(timeout)
        .timeout(timeout)
        .connect()
        .await
        .with_context(|| format!("failed to connect to {endpoint}"))?;
    let response = HealthClient::new(channel)
        .check(HealthCheckRequest {
            service: service.to_owned(),
        })
        .await
        .with_context(|| format!("the health check of {endpoint} failed"))?;
    match response.into_inner().status() {
        ServingStatus::Serving => {
            println!("SERVING");
            Ok(())
        }
        status => bail!("{endpoint} is {}", status.as_str_name()),
    }
}
