use bytes::Bytes;
use http::Method;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{Request, Response, body::Incoming, service::Service, upgrade::Upgraded};
use hyper_util::rt::TokioIo;
use std::{io, net::SocketAddr, pin::Pin, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{Sender, channel},
        oneshot,
    },
    task::JoinHandle,
};

use crate::logger::{DevNullLogger, LogEntry, VSCodeRestLogger, VSCodeRestLoggerOptions};
use crate::{
    config::{Config, LoggerType},
    logger::RequestResponseLogger,
};

mod body;
mod config;
mod logger;

type ClientBuilder = hyper::client::conn::http1::Builder;

const FALLBACK_TARGET_PORT: u16 = 80;
const CHANNEL_BUFFER_SIZE: usize = 64;
/// How long to wait for open connections to finish on shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// The main entry point of the Request Logging Proxy application.
/// It sets up the proxy server, handles incoming connections,
/// and manages graceful shutdown on receiving a termination signal.
#[tokio::main]
async fn main() {
    let config = Config::from_args();

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = TcpListener::bind(addr)
        .await
        .expect("Failed to bind to address");

    eprintln!("Proxy Server starting on port {}...", addr.port());
    eprintln!("Targeting all requests to: {}", config.target_url);
    let target_address = config
        .target_url
        .socket_addrs(|| Some(FALLBACK_TARGET_PORT))
        .expect("Unable to resolve target address")[0];

    let log_channel = match config.logger {
        LoggerType::DevNull => log_channel(DevNullLogger),
        LoggerType::Vscode => {
            if let Some(dir) = &config.binary_body_dir {
                std::fs::create_dir_all(dir).expect("Unable to create binary body directory");
            }
            log_channel(VSCodeRestLogger::from_stdout(
                io::stdout(),
                VSCodeRestLoggerOptions {
                    max_text_body_bytes: config.max_logged_body_bytes,
                    binary_body_dir: config.binary_body_dir,
                },
            ))
        }
    };
    let logger = ReplayLogger::new(
        log_channel.sender.clone(),
        target_address,
        config.override_bearer_auth,
    );

    let server = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .preserve_header_case(true)
        .title_case_headers(true);

    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut signal = std::pin::pin!(shutdown_signal());

    loop {
        tokio::select! {
            Ok((stream, _addr)) = listener.accept() => {
                let io = TokioIo::new(stream);
                let conn = server.serve_connection_with_upgrades(io, logger.clone());
                // watch this connection
                let fut = graceful.watch(conn.into_owned());
                tokio::spawn(async move {
                    if let Err(e) = fut.await {
                        eprintln!("Error serving connection: {:?}", e);
                    }
                });
            },

            signal_str = &mut signal => {
                drop(listener);
                eprintln!("Graceful shutdown signal received: {}", signal_str);
                // stop the accept loop
                break;
            }
        }
    }

    eprintln!("Waiting for open connections to finish (press CTRL+C again to skip)...");
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(SHUTDOWN_TIMEOUT) => {
            eprintln!("Timed out after {SHUTDOWN_TIMEOUT:?} waiting for open connections");
        }
        signal_str = shutdown_signal() => {
            eprintln!("Second shutdown signal received: {signal_str}");
        }
    }

    drop(logger);
    log_channel.close().await;
    eprintln!("All log entries written");
}

/// A channel to a background task that writes log entries.
struct LogChannel {
    sender: Sender<LogEntry>,
    close: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl LogChannel {
    /// Stop accepting new entries and wait until all queued entries are written.
    async fn close(self) {
        let Self {
            sender,
            close,
            task,
        } = self;
        drop(sender);
        // the task may already have finished if all senders are gone
        let _ = close.send(());
        if let Err(e) = task.await {
            eprintln!("Logger task failed: {e}");
        }
    }
}

/// Create a logging channel that processes request/response pairs sequentially.
/// This makes sure that log entries are not interleaved.
/// The channel spawns a background task that receives request/response pairs
/// and calls the provided logger's `log_request_response` method.
fn log_channel<L: RequestResponseLogger + 'static>(mut logger: L) -> LogChannel {
    let (sender, mut rx) = channel::<LogEntry>(CHANNEL_BUFFER_SIZE);
    let (close, mut close_rx) = oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                entry = rx.recv() => match entry {
                    Some(entry) => log_entry(&mut logger, entry).await,
                    // all senders are gone, so nothing is queued anymore
                    None => return,
                },
                _ = &mut close_rx => break,
            }
        }

        // Reject new entries, but write all entries that are already queued.
        rx.close();
        while let Some(entry) = rx.recv().await {
            log_entry(&mut logger, entry).await;
        }
    });

    LogChannel {
        sender,
        close,
        task,
    }
}

async fn log_entry<L: RequestResponseLogger>(logger: &mut L, entry: LogEntry) {
    if let Err(e) = logger.log_request_response(entry).await {
        eprintln!("Error logging request/response: {e:?}");
    }
}

/// A service that proxies requests to a target address
/// and logs the request/response pairs using a logging channel.
#[derive(Clone, Debug)]
struct ReplayLogger {
    logger: Sender<LogEntry>,
    target: SocketAddr,
    override_bearer_auth: Option<String>,
}

impl Service<Request<Incoming>> for ReplayLogger {
    type Response = Response<Full<Bytes>>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        Box::pin(self.clone().handle_request(req))
    }
}

impl ReplayLogger {
    fn new(
        logger: Sender<LogEntry>,
        target: SocketAddr,
        override_bearer_auth: Option<String>,
    ) -> Self {
        Self {
            logger,
            target,
            override_bearer_auth,
        }
    }

    async fn handle_request(
        self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        let mut request = {
            let (parts, body) = req.into_parts();
            let full_body_bytes = body.collect().await?.to_bytes();
            Request::from_parts(parts, Full::from(full_body_bytes.clone()))
        };

        if let Some(token) = &self.override_bearer_auth {
            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
            );
        }

        let response = {
            let response = proxy(self.target, request.clone()).await?;
            let (parts, body) = response.into_parts();
            let full_body_bytes = body.collect().await?.to_bytes();
            Response::from_parts(parts, Full::from(full_body_bytes.clone()))
        };

        if self
            .logger
            .send(LogEntry {
                request,
                response: response.clone(),
            })
            .await
            .is_err()
        {
            eprintln!("Logger is shut down, dropping log entry");
        }

        Ok(response)
    }
}

/// Wait for the shutdown signal (CTRL+C or SIGTERM)
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm =
        signal(SignalKind::terminate()).expect("failed to install SIGTERM signal handler");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            "SIGINT"
        }
        _ = sigterm.recv() => {
            "SIGTERM"
        }
    }
}

/// Received an HTTP request like:
/// ```
/// CONNECT www.domain.com:443 HTTP/1.1
/// Host: www.domain.com:443
/// Proxy-Connection: Keep-Alive
/// ```
///
/// When HTTP method is CONNECT we should return an empty body
/// then we can eventually upgrade the connection and talk a new protocol.
///
/// Note: only after client received an empty body with STATUS_OK can the
/// connection be upgraded, so we can't return a response inside
/// `on_upgrade` future.
async fn proxy_connect(
    req: Request<Full<Bytes>>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    assert_eq!(Method::CONNECT, req.method());

    let Some(addr) = host_addr(req.uri()) else {
        eprintln!("CONNECT host is not socket addr: {:?}", req.uri());
        let mut resp = Response::new(full("CONNECT must be to a socket address"));
        *resp.status_mut() = http::StatusCode::BAD_REQUEST;

        return Ok(resp);
    };

    tokio::task::spawn(async move {
        let upgraded = match hyper::upgrade::on(req).await {
            Ok(upgraded) => upgraded,
            Err(e) => {
                eprintln!("upgrade error: {e}");
                return;
            }
        };
        if let Err(e) = tunnel(upgraded, addr).await {
            eprintln!("server IO error: {e}");
        };
    });

    Ok(Response::new(empty()))
}

/// Proxy the request to the target address
/// and return the response.
async fn proxy(
    addrs: SocketAddr,
    req: Request<Full<Bytes>>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    if Method::CONNECT == req.method() {
        return proxy_connect(req).await;
    }

    let stream = TcpStream::connect(addrs).await.unwrap();
    let io = TokioIo::new(stream);

    let (mut sender, conn) = ClientBuilder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await?;

    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            eprintln!("Connection failed: {err:?}");
        }
    });

    let resp = sender.send_request(req).await?;
    Ok(resp.map(|b| b.boxed()))
}

/// Extract the host:port from the URI
fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

/// Create an empty body
fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// Create a full body from the given chunk
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

/// Create a TCP connection to host:port, build a tunnel between the connection and
/// the upgraded connection
async fn tunnel(upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    let (_from_client, _from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn it_overrides_auth_with_bearer_auth() {
        let uri = http::Uri::from_str("http://example.com/api/test").unwrap();

        // Test 1: Setting auth header on request without one
        let mut request: Request<Full<Bytes>> = Request::builder()
            .method("GET")
            .uri(uri.clone())
            .body(Full::new(Bytes::new()))
            .unwrap();

        assert!(request.headers().get(http::header::AUTHORIZATION).is_none());

        let test_token = "test-secret-token-12345";
        if let Some(token) = Some(test_token.to_string()) {
            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
            );
        }

        assert_eq!(
            request.headers().get(http::header::AUTHORIZATION).unwrap(),
            "Bearer test-secret-token-12345"
        );

        // Test 2: Replacing existing auth header
        let mut request: Request<Full<Bytes>> = Request::builder()
            .method("GET")
            .uri(uri.clone())
            .header(http::header::AUTHORIZATION, "Bearer old-token")
            .body(Full::new(Bytes::new()))
            .unwrap();

        assert_eq!(
            request.headers().get(http::header::AUTHORIZATION).unwrap(),
            "Bearer old-token"
        );

        let new_token = "new-test-token";
        if let Some(token) = Some(new_token.to_string()) {
            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
            );
        }

        assert_eq!(
            request.headers().get(http::header::AUTHORIZATION).unwrap(),
            "Bearer new-test-token"
        );

        // Test 3: Bearer token format validation
        let jwt_token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let expected = format!("Bearer {}", jwt_token);
        let header_value = http::HeaderValue::from_str(&expected).unwrap();

        assert_eq!(header_value, expected);
        assert!(header_value.to_str().unwrap().starts_with("Bearer "));
    }
}
