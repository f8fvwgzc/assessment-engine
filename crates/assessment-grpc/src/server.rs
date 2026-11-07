//! Serving [`AssessmentService`] the way the `assessment-engine` binary does:
//! with `grpc.health.v1`, server reflection (v1 and v1alpha), optional
//! bearer-token auth on the engine API, message-size, timeout and global
//! concurrency limits, and graceful shutdown.
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use assessment_grpc::server::{ServerOptions, serve_with_incoming_shutdown};
//! use assessment_grpc::service::AssessmentService;
//!
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
//! let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
//! serve_with_incoming_shutdown(
//!     AssessmentService::in_memory(),
//!     ServerOptions::default(),
//!     incoming,
//!     async { let _ = tokio::signal::ctrl_c().await; },
//! )
//! .await?;
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::future::Future;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_stream::Stream;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Server;
use tonic::transport::server::Connected;
use tower::limit::GlobalConcurrencyLimitLayer;

use crate::auth::BearerAuth;
use crate::pb;
use crate::pb::assessment_engine_server::AssessmentEngineServer;
use crate::service::AssessmentService;

/// The engine API as served: size limits applied, behind [`BearerAuth`].
pub type EngineServer = InterceptedService<AssessmentEngineServer<AssessmentService>, BearerAuth>;

/// How to serve.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Largest message decoded or encoded, in bytes. Default 16 MiB.
    pub max_message_bytes: usize,
    /// Deadline for each call (`None` = none). Default 30 s.
    pub request_timeout: Option<Duration>,
    /// Most calls served at once across all connections; more wait. Default 256.
    pub concurrency_limit: usize,
    /// Authentication of the engine API (health and reflection stay open).
    /// Default: disabled.
    pub auth: BearerAuth,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_message_bytes: 16 * 1024 * 1024,
            request_timeout: Some(Duration::from_secs(30)),
            concurrency_limit: 256,
            auth: BearerAuth::disabled(),
        }
    }
}

impl ServerOptions {
    /// These options with another message-size limit.
    #[must_use]
    pub fn with_max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = bytes;
        self
    }

    /// These options with another per-call deadline.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// These options with another concurrency limit (at least 1).
    #[must_use]
    pub fn with_concurrency_limit(mut self, limit: usize) -> Self {
        self.concurrency_limit = limit.max(1);
        self
    }

    /// These options with another authentication.
    #[must_use]
    pub fn with_auth(mut self, auth: BearerAuth) -> Self {
        self.auth = auth;
        self
    }
}

/// What went wrong while serving.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeErrorKind {
    /// The reflection service could not be built from the descriptor sets.
    Reflection,
    /// The transport failed (accepting connections, HTTP/2).
    Transport,
}

/// The error [`serve_with_incoming_shutdown`] returns.
#[derive(Debug)]
pub struct ServeError {
    kind: ServeErrorKind,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl ServeError {
    /// What kind of failure this is.
    pub fn kind(&self) -> ServeErrorKind {
        self.kind
    }

    fn new(kind: ServeErrorKind, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Box::new(source),
        }
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ServeErrorKind::Reflection => f.write_str("the reflection service could not be built"),
            ServeErrorKind::Transport => f.write_str("the grpc transport failed"),
        }
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// The engine API with `options`' size limits and auth applied.
pub fn engine_server(service: AssessmentService, options: &ServerOptions) -> EngineServer {
    let server = AssessmentEngineServer::new(service)
        .max_decoding_message_size(options.max_message_bytes)
        .max_encoding_message_size(options.max_message_bytes);
    InterceptedService::new(server, options.auth.clone())
}

/// Serves `service` (with health and reflection) on `incoming` until
/// `shutdown` completes; then health reports `NOT_SERVING`, the listener is
/// dropped and open calls drain.
pub async fn serve_with_incoming_shutdown<I, IO, IE, F>(
    service: AssessmentService,
    options: ServerOptions,
    incoming: I,
    shutdown: F,
) -> Result<(), ServeError>
where
    I: Stream<Item = Result<IO, IE>>,
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    IE: Into<Box<dyn std::error::Error + Send + Sync>>,
    F: Future<Output = ()>,
{
    let reflection_v1 = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(pb::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
        .build_v1()
        .map_err(|e| ServeError::new(ServeErrorKind::Reflection, e))?;
    let reflection_v1alpha = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(pb::FILE_DESCRIPTOR_SET)
        .register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
        .build_v1alpha()
        .map_err(|e| ServeError::new(ServeErrorKind::Reflection, e))?;

    let (reporter, health) = tonic_health::server::health_reporter();
    reporter
        .set_serving::<AssessmentEngineServer<AssessmentService>>()
        .await;

    let engine = engine_server(service, &options);
    let mut builder = Server::builder()
        .http2_keepalive_interval(Some(Duration::from_secs(60)))
        .http2_keepalive_timeout(Some(Duration::from_secs(20)))
        .trace_fn(|request| tracing::info_span!("grpc", path = %request.uri().path()));
    if let Some(timeout) = options.request_timeout {
        builder = builder.timeout(timeout);
    }
    builder
        .layer(GlobalConcurrencyLimitLayer::new(
            options.concurrency_limit.max(1),
        ))
        .add_service(health)
        .add_service(reflection_v1)
        .add_service(reflection_v1alpha)
        .add_service(engine)
        .serve_with_incoming_shutdown(incoming, async move {
            shutdown.await;
            reporter
                .set_not_serving::<AssessmentEngineServer<AssessmentService>>()
                .await;
        })
        .await
        .map_err(|e| ServeError::new(ServeErrorKind::Transport, e))
}
