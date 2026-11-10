//! The `serve` subcommand: the registry, the service, the listener and the
//! shutdown signals.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use assessment_core::Registry;
use assessment_core::registry::RegistryOptions;
use assessment_grpc::auth::BearerAuth;
use assessment_grpc::server::{ServerOptions, serve_with_incoming_shutdown};
use assessment_grpc::service::{AssessmentService, ServiceLimits};
use tokio::sync::watch;
use tonic::transport::server::TcpIncoming;

use crate::{ServeArgs, engine_with_policy, judge};

/// Serves until SIGINT or SIGTERM.
pub(crate) async fn run(args: ServeArgs) -> Result<()> {
    run_until(args, shutdown_signal(), None).await
}

/// Serves until `shutdown` completes, then compacts the journal (see
/// [`compact_on_shutdown`]). Once the server listens, its address is sent
/// to `listening`.
async fn run_until(
    args: ServeArgs,
    shutdown: impl Future<Output = ()> + Send + 'static,
    listening: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> Result<()> {
    let engine = engine_with_policy(args.policy.as_deref())?;
    let auth = BearerAuth::from_option(args.auth_token.as_deref());
    // Checked before the journal is opened: a bad judge setup fails fast.
    let judge = judge::from_args(
        &args.judge,
        judge::key_from_env(),
        args.addr,
        auth.is_enabled(),
    )?;
    // 0 turns compaction off, on shutdown too: the journal then stays in
    // the one-file format that older versions read.
    let compact_after_ops = (args.compact_after_ops > 0).then_some(args.compact_after_ops);
    let registry = match &args.journal {
        Some(path) => {
            let options = RegistryOptions {
                fsync: args.fsync,
                history_limit: args.history_limit,
                compact_after_ops,
            };
            // Replaying a long journal takes a while; keep it off the
            // async threads.
            let (engine, journal) = (engine.clone(), path.clone());
            let registry =
                tokio::task::spawn_blocking(move || Registry::open_with(engine, journal, options))
                    .await
                    .context("the task that opens the registry journal failed")?
                    .with_context(|| {
                        format!("failed to open the registry journal `{}`", path.display())
                    })?;
            tracing::info!(
                journal = %path.display(),
                fsync = args.fsync,
                entities = registry.len(),
                journal_ops = registry.journal_ops(),
                compact_after_ops = args.compact_after_ops,
                history_limit = ?args.history_limit,
                "registry journal replayed"
            );
            registry
        }
        None => {
            tracing::info!(
                history_limit = ?args.history_limit,
                "registry in memory (no --journal)"
            );
            let registry = Registry::new(engine.clone());
            match args.history_limit {
                Some(n) => registry.with_history_limit(n),
                None => registry,
            }
        }
    };
    let registry = Arc::new(registry);

    let mut limits = ServiceLimits::default().with_max_batch_size(args.max_batch_size);
    if let Some(n) = args.max_concurrent_assessments {
        limits = limits.with_max_concurrent_assessments(n);
    }
    if let Some(n) = args.max_in_flight {
        limits = limits.with_max_in_flight(n);
    }
    // The signal turns this to `true` first, so that calls in flight stop
    // asking the judge and answer with what they have.
    let (shutting_down, shutdown_seen) = watch::channel(false);
    let shutdown = async move {
        shutdown.await;
        let _ = shutting_down.send(true);
    };
    let mut service = AssessmentService::new(engine, Arc::clone(&registry))
        .with_limits(limits)
        .with_shutdown(shutdown_seen);
    if let Some(stack) = judge {
        stack.spawn_startup_probe();
        service = service.with_judge(stack.judge, stack.settings);
    }
    // The values in force (the service brings them into range).
    let limits = service.limits();

    let judge_id = service.judge_id().unwrap_or("off").to_owned();
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
        judge = %judge_id,
        engine_version = assessment_core::ENGINE_VERSION,
        "assessment engine listening (gRPC, health and reflection on)"
    );

    if let Some(listening) = listening {
        // Nobody waiting for the address is fine.
        let _ = listening.send(local);
    }

    let served = serve_with_incoming_shutdown(service, options, incoming, shutdown)
        .await
        .with_context(|| format!("failed to serve on {local}"));
    if args.journal.is_some() && compact_after_ops.is_some() {
        compact_on_shutdown(registry).await;
    }
    served?;
    tracing::info!("assessment engine stopped");
    Ok(())
}

/// Compacts the journal once the server has stopped, so that the next start
/// loads a snapshot and does not replay every operation.
///
/// A failure is logged and does not fail the shutdown: the journal the
/// registry had is still complete.
async fn compact_on_shutdown(registry: Arc<Registry>) {
    if registry.journal_ops() == 0 {
        return;
    }
    match tokio::task::spawn_blocking(move || registry.compact()).await {
        Ok(Ok(Some(done))) => tracing::info!(
            generation = done.generation,
            entries = done.entries,
            snapshot_bytes = done.snapshot_bytes,
            "registry journal compacted on shutdown"
        ),
        Ok(Ok(None)) => {}
        Ok(Err(err)) => {
            tracing::error!(error = %err, "failed to compact the registry journal on shutdown");
        }
        Err(err) => {
            tracing::error!(error = %err, "the shutdown compaction task failed");
        }
    }
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use assessment_core::model::Entity;
    use assessment_core::registry::{ImportItem, PutOptions};
    use assessment_grpc::client::AssessmentClient;
    use clap::Parser;
    use tokio::sync::oneshot;

    use super::*;
    use crate::Cli;

    /// A journal path of its own for one test; files left by an earlier run
    /// are removed.
    fn journal_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("assessment-server-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temporary directory is created");
        dir.join("registry.journal")
    }

    fn snapshot_path(journal: &Path) -> PathBuf {
        let mut name = journal.as_os_str().to_owned();
        name.push(".snapshot");
        PathBuf::from(name)
    }

    fn serve_args(journal: &Path, extra: &[&str]) -> ServeArgs {
        let journal = journal.to_str().expect("the path is UTF-8");
        let mut argv = vec![
            "assessment-engine",
            "--addr",
            "127.0.0.1:0",
            "--journal",
            journal,
        ];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv).expect("parses").serve
    }

    /// A server started from `args`, a client connected to it, and the way
    /// to stop it.
    struct Running {
        client: AssessmentClient,
        stop: oneshot::Sender<()>,
        server: tokio::task::JoinHandle<Result<()>>,
    }

    impl Running {
        async fn start(args: ServeArgs) -> Self {
            let (stop, stopped) = oneshot::channel::<()>();
            let (listening, address) = oneshot::channel();
            let server = tokio::spawn(run_until(
                args,
                async {
                    let _ = stopped.await;
                },
                Some(listening),
            ));
            let address = address.await.expect("the server listens");
            let client = AssessmentClient::connect(format!("http://{address}"))
                .await
                .expect("the client connects");
            Self {
                client,
                stop,
                server,
            }
        }

        /// A clean shutdown, as on SIGTERM.
        async fn shutdown(self) {
            drop(self.client);
            let _ = self.stop.send(());
            self.server
                .await
                .expect("the server task finishes")
                .expect("the server stops cleanly");
        }
    }

    /// What a client can read back about the test entities.
    async fn observed(client: &mut AssessmentClient) -> String {
        let mut out = String::new();
        for id in ["ENG-1", "ENG-2", "ENG-3"] {
            // Errors are shown without their cause: a status carries the
            // response's `date` header, which differs between two calls.
            let entity = client.get_entity(id, None).await.map_err(|e| e.to_string());
            let history = client.history(id, 0).await.map_err(|e| e.to_string());
            out.push_str(&format!("{id}: {entity:?}\n{history:?}\n"));
        }
        let stale = client.stale(None).await.expect("stale answers");
        out.push_str(&format!("stale: {stale:?}\n"));
        out
    }

    async fn fill(client: &mut AssessmentClient) {
        let issue = |id: &str, title: &str| Entity::new(id, "issue").with_title(title);
        let report = client
            .import_iter(vec![
                ImportItem::upsert(issue("ENG-1", "Rate limits for the public API")),
                ImportItem::upsert(issue("ENG-2", "Usage dashboard")),
                ImportItem::upsert(issue("ENG-3", "Billing export")),
            ])
            .await
            .expect("the import runs");
        assert_eq!(report.summary.created, 3, "{report:?}");
        client
            .put_entity(
                issue("ENG-1", "Rate limits for the public and the partner API"),
                PutOptions::default(),
            )
            .await
            .expect("the put runs");
        client
            .delete_entity("ENG-3", false, None, None)
            .await
            .expect("the delete runs");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_after_the_shutdown_compaction_serves_the_same_state() {
        let journal = journal_path("shutdown-compaction");

        let mut first = Running::start(serve_args(&journal, &[])).await;
        fill(&mut first.client).await;
        let before = observed(&mut first.client).await;
        assert!(!snapshot_path(&journal).exists(), "nothing compacted yet");
        first.shutdown().await;

        assert!(
            snapshot_path(&journal).exists(),
            "a clean shutdown writes the snapshot"
        );
        let mut second = Running::start(serve_args(&journal, &[])).await;
        assert_eq!(observed(&mut second.client).await, before);
        // The restarted server still takes writes on top of the snapshot.
        second
            .client
            .put_entity(
                Entity::new("ENG-2", "issue").with_title("Usage dashboard for admins"),
                PutOptions::default(),
            )
            .await
            .expect("the put runs");
        let after_write = observed(&mut second.client).await;
        assert_ne!(after_write, before);
        second.shutdown().await;

        let mut third = Running::start(serve_args(&journal, &[])).await;
        assert_eq!(observed(&mut third.client).await, after_write);
        third.shutdown().await;
        let _ = std::fs::remove_dir_all(journal.parent().expect("the journal has a directory"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_journal_compacts_while_serving_once_it_holds_enough_operations() {
        let journal = journal_path("live-compaction");

        let mut first = Running::start(serve_args(&journal, &["--compact-after-ops", "3"])).await;
        fill(&mut first.client).await;
        assert!(
            snapshot_path(&journal).exists(),
            "five operations pass the limit of three"
        );
        let before = observed(&mut first.client).await;
        first.shutdown().await;

        let mut second = Running::start(serve_args(&journal, &["--compact-after-ops", "3"])).await;
        assert_eq!(observed(&mut second.client).await, before);
        second.shutdown().await;
        let _ = std::fs::remove_dir_all(journal.parent().expect("the journal has a directory"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compact_after_ops_zero_never_writes_a_snapshot() {
        let journal = journal_path("no-compaction");

        let mut first = Running::start(serve_args(&journal, &["--compact-after-ops", "0"])).await;
        fill(&mut first.client).await;
        let before = observed(&mut first.client).await;
        first.shutdown().await;
        assert!(!snapshot_path(&journal).exists());

        let mut second = Running::start(serve_args(&journal, &["--compact-after-ops", "0"])).await;
        assert_eq!(observed(&mut second.client).await, before);
        second.shutdown().await;
        let _ = std::fs::remove_dir_all(journal.parent().expect("the journal has a directory"));
    }

    /// The judge points at a local port nothing listens on, so the startup
    /// check fails and is only logged.
    #[cfg(feature = "judge")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_with_a_judge_names_it_and_one_without_does_not() {
        let journal = journal_path("judge");
        let key_file = journal.with_file_name("key");
        std::fs::write(&key_file, "test-key-0123456789\n").expect("the key file is written");

        let mut plain = Running::start(serve_args(&journal, &["--judge", "off"])).await;
        assert_eq!(plain.client.judge_info().await.expect("answers"), None);
        plain.shutdown().await;

        let args = serve_args(
            &journal,
            &[
                "--judge",
                "on",
                "--judge-model",
                "claude-haiku-4-5",
                "--judge-base-url",
                "http://127.0.0.1:9",
                "--anthropic-api-key-file",
                key_file.to_str().expect("the path is UTF-8"),
            ],
        );
        let mut judged = Running::start(args).await;
        assert_eq!(
            judged.client.judge_info().await.expect("answers"),
            Some("anthropic/claude-haiku-4-5".to_owned())
        );
        judged.shutdown().await;
        let _ = std::fs::remove_dir_all(journal.parent().expect("the journal has a directory"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_limit_drops_the_content_of_older_versions() {
        let journal = journal_path("history-limit");

        let mut server = Running::start(serve_args(&journal, &["--history-limit", "1"])).await;
        fill(&mut server.client).await;
        let history = server
            .client
            .history("ENG-1", 0)
            .await
            .expect("history answers");
        assert_eq!(history.len(), 2, "{history:?}");
        let first_version = history[0].version;
        let err = server
            .client
            .get_entity("ENG-1", Some(first_version))
            .await
            .expect_err("the first version is no longer kept");
        assert_eq!(err.code(), Some(tonic::Code::NotFound), "{err}");
        server
            .client
            .get_entity("ENG-1", None)
            .await
            .expect("the current version is kept");
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(journal.parent().expect("the journal has a directory"));
    }
}
