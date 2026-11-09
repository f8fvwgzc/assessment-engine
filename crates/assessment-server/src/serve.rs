//! The `serve` subcommand: the registry, the service, the listener and the
//! shutdown signals.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use assessment_core::Registry;
use assessment_grpc::auth::BearerAuth;
use assessment_grpc::server::{ServerOptions, serve_with_incoming_shutdown};
use assessment_grpc::service::{AssessmentService, ServiceLimits};
use tonic::transport::server::TcpIncoming;

use crate::{ServeArgs, engine_with_policy};

/// Serves until SIGINT or SIGTERM.
pub(crate) async fn run(args: ServeArgs) -> Result<()> {
    let engine = engine_with_policy(args.policy.as_deref())?;
    let registry = match &args.journal {
        Some(path) => {
            let registry = Registry::open(engine.clone(), path, args.fsync).with_context(|| {
                format!("failed to open the registry journal `{}`", path.display())
            })?;
            tracing::info!(
                journal = %path.display(),
                fsync = args.fsync,
                entities = registry.len(),
                "registry journal replayed"
            );
            registry
        }
        None => {
            tracing::info!("registry in memory (no --journal)");
            Registry::new(engine.clone())
        }
    };

    let mut limits = ServiceLimits::default().with_max_batch_size(args.max_batch_size);
    if let Some(n) = args.max_concurrent_assessments {
        limits = limits.with_max_concurrent_assessments(n);
    }
    if let Some(n) = args.max_in_flight {
        limits = limits.with_max_in_flight(n);
    }
    let service = AssessmentService::new(engine, Arc::new(registry)).with_limits(limits);
    // The values in force (the service brings them into range).
    let limits = service.limits();

    let auth = BearerAuth::from_option(args.auth_token.as_deref());
    let timeout =
        (args.request_timeout_secs > 0).then(|| Duration::from_secs(args.request_timeout_secs));
    let options = ServerOptions::default()
        .with_max_message_bytes(args.max_message_bytes)
        .with_request_timeout(timeout)
        .with_concurrency_limit(args.concurrency_limit)
        .with_auth(auth.clone());

    let incoming = TcpIncoming::bind(args.addr)
        .with_context(|| format!("failed to listen on {}", args.addr))?
        .with_nodelay(Some(true))
        .with_keepalive(Some(Duration::from_secs(60)));
    let local = incoming
        .local_addr()
        .context("failed to read the listening address")?;
    tracing::info!(
        addr = %local,
        auth = auth.is_enabled(),
        max_message_bytes = args.max_message_bytes,
        request_timeout_secs = args.request_timeout_secs,
        concurrency_limit = args.concurrency_limit,
        max_batch_size = limits.max_batch_size,
        max_in_flight = limits.max_in_flight,
        max_concurrent_assessments = limits.max_concurrent_assessments,
        engine_version = assessment_core::ENGINE_VERSION,
        "assessment engine listening (gRPC, health and reflection on)"
    );

    serve_with_incoming_shutdown(service, options, incoming, shutdown_signal())
        .await
        .with_context(|| format!("failed to serve on {local}"))?;
    tracing::info!("assessment engine stopped");
    Ok(())
}

/// Completes on SIGINT (Ctrl-C) or SIGTERM.
async fn shutdown_signal() {
    let interrupt = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "failed to listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("SIGINT received, shutting down"),
        () = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}
