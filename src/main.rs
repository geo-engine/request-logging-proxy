use bytes::Bytes;
use http::Method;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::upgrade::Upgraded;
use hyper::{Request, Response, body::Incoming, service::Service};
use hyper_util::rt::TokioIo;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc::{Sender, channel},
};

use crate::logger::{LogEntry, VSCodeRestLogger};
use crate::{
    config::{Config, LoggerType},
    logger::RequestResponseLogger,
};

mod config;
mod logger;

type ClientBuilder = hyper::client::conn::http1::Builder;

const FALLBACK_TARGET_PORT: u16 = 80;
const CHANNEL_BUFFER_SIZE: usize = 64;

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

    let log_sender = match config.logger {
        LoggerType::Vscode => log_channel(VSCodeRestLogger::from_stdout(io::stdout())),
    };
    let logger = ReplayLogger::new(log_sender, target_address);

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

            _ = &mut signal => {
                drop(listener);
                eprintln!("Graceful shutdown signal received");
                // stop the accept loop
                break;
            }
        }
    }
}

/// Create a logging channel that processes request/response pairs sequentially.
/// This makes sure that log entries are not interleaved.
/// The channel spawns a background task that receives request/response pairs
/// and calls the provided logger's `log_request_response` method.
fn log_channel<L: RequestResponseLogger + 'static>(mut logger: L) -> Sender<LogEntry> {
    let (tx, mut rx) = channel::<LogEntry>(CHANNEL_BUFFER_SIZE);

    tokio::spawn(async move {
        while let Some(entry) = rx.recv().await {
            if let Err(e) = logger.log_request_response(entry).await {
                eprintln!("Error logging request/response: {e:?}");
            }
        }
    });

    tx
}

/// A service that proxies requests to a target address
/// and logs the request/response pairs using a logging channel.
#[derive(Clone, Debug)]
struct ReplayLogger {
    logger: Sender<LogEntry>,
    target: SocketAddr,
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
    fn new(logger: Sender<LogEntry>, target: SocketAddr) -> Self {
        Self { logger, target }
    }

    async fn handle_request(
        self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        let request = {
            let (parts, body) = req.into_parts();
            let full_body_bytes = body.collect().await?.to_bytes();
            Request::from_parts(parts, Full::from(full_body_bytes.clone()))
        };

        let response = {
            let response = proxy(self.target, request.clone()).await?;
            let (parts, body) = response.into_parts();
            let full_body_bytes = body.collect().await?.to_bytes();
            Response::from_parts(parts, Full::from(full_body_bytes.clone()))
        };

        self.logger
            .send(LogEntry {
                request,
                response: response.clone(),
            })
            .await
            .unwrap();

        Ok(response)
    }
}

/// Wait for the shutdown signal (CTRL+C)
async fn shutdown_signal() {
    // Wait for the CTRL+C signal
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
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

// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[tokio::test]
//     async fn it_forwards_and_logs_requests() {
//         let (logger, _handle) = create_logger();
//     }
// }
