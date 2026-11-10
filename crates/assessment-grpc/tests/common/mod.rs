//! A real server on `127.0.0.1:0` for end-to-end tests.
#![allow(dead_code)]

use std::time::Duration;

use assessment_grpc::client::AssessmentClient;
use assessment_grpc::server::{ServeError, ServerOptions, serve_with_incoming_shutdown};
use assessment_grpc::service::AssessmentService;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::TcpListenerStream;

pub struct TestServer {
    pub url: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), ServeError>>>,
}

impl TestServer {
    pub async fn start(options: ServerOptions) -> Self {
        Self::start_with(AssessmentService::in_memory(), options).await
    }

    pub async fn start_with(service: AssessmentService, options: ServerOptions) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds a free port");
        let addr = listener.local_addr().expect("has an address");
        let incoming = TcpListenerStream::new(listener);
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_with_incoming_shutdown(
            service,
            options,
            incoming,
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            url: format!("http://{addr}"),
            stop: Some(stop),
            task: Some(task),
        }
    }

    pub async fn client(&self) -> AssessmentClient {
        AssessmentClient::connect(self.url.clone())
            .await
            .expect("connects")
    }

    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let result = tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("shuts down in time")
                .expect("the server task does not panic");
            result.expect("serves without error");
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
