//! # assessment-grpc
//!
//! The gRPC layer of the assessment engine: the `assessment.v1` contract
//! (`proto/assessment/v1/assessment.proto`) compiled with tonic, lossless
//! conversions to and from [`assessment_core`], a tonic service over the
//! core [`Engine`](assessment_core::Engine) and
//! [`Registry`](assessment_core::Registry), and a client that speaks core
//! types.
//!
//! | Module | Feature | What it is |
//! |---|---|---|
//! | [`pb`] | always | The generated messages, client and server traits, and the encoded descriptor set for reflection. |
//! | [`convert`] | always | `core ↔ pb` conversions; proto → core errors name the offending field. |
//! | [`service`] | `server` | [`AssessmentService`](service::AssessmentService), the `AssessmentEngine` implementation. |
//! | [`auth`] | `server` | [`BearerAuth`](auth::BearerAuth), a constant-time bearer-token interceptor. |
//! | [`server`] | `server` | Serving with health, reflection, limits, auth and graceful shutdown. |
//! | [`client`] | `client` | [`AssessmentClient`](client::AssessmentClient), which takes and returns core types. |
//!
//! ## Serving
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//!
//! use assessment_core::{Engine, Registry};
//! use assessment_grpc::auth::BearerAuth;
//! use assessment_grpc::server::{ServerOptions, serve_with_incoming_shutdown};
//! use assessment_grpc::service::AssessmentService;
//!
//! let registry = Arc::new(Registry::new(Engine::default()));
//! let service = AssessmentService::from_registry(registry);
//! let options = ServerOptions::default().with_auth(BearerAuth::new("s3cret"));
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:50061").await?;
//! let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
//! serve_with_incoming_shutdown(service, options, incoming, async {
//!     let _ = tokio::signal::ctrl_c().await;
//! })
//! .await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Calling
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use assessment_core::prelude::*;
//! use assessment_grpc::client::AssessmentClient;
//!
//! let mut client = AssessmentClient::connect("http://127.0.0.1:50061")
//!     .await?
//!     .with_bearer_token("s3cret")?;
//! let parent = Entity::new("ENG-1", "issue")
//!     .with_title("Authentication")
//!     .with_description("- [ ] Email and password login\n- [ ] Password reset by email")
//!     .with_version(SemVer::new(1, 0, 0));
//! let rewritten = parent.clone().with_description("- [ ] Google OAuth login only");
//! let assessment = client
//!     .assess(AssessmentRequest {
//!         changes: vec![ChangeSet::update(parent, rewritten)],
//!         ..Default::default()
//!     })
//!     .await?;
//! println!("{:?} → {:?}", assessment.recommendation, assessment.changes[0].next_version);
//! # Ok(())
//! # }
//! ```

/// The generated `assessment.v1` messages, enums, client and server.
#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod pb {
    tonic::include_proto!("assessment.v1");

    /// The encoded `FileDescriptorSet` of `assessment.proto` and its imports,
    /// for gRPC server reflection.
    pub const FILE_DESCRIPTOR_SET: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/assessment_descriptor.bin"));
}

pub mod convert;

#[cfg(feature = "server")]
pub mod auth;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod service;

#[cfg(feature = "client")]
pub mod client;
