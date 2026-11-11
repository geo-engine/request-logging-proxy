//! A small upstream server for trying out the proxy.
//!
//! It serves the kinds of responses an OGC API server returns:
//! compressed JSON, image tiles, GeoTIFF coverages, legacy charsets and bodiless responses.
//!
//! ```bash
//! cargo run --example demo_upstream -- 8484
//! ```
//!
//! See `examples/demo.sh` for a complete run with the proxy in front of it.

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 8484;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let port = std::env::args()
        .nth(1)
        .and_then(|port| port.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).await?;
    eprintln!("Demo upstream listening on http://{addr}");

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            let server = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            if let Err(e) = server
                .serve_connection(TokioIo::new(stream), service_fn(handle))
                .await
            {
                eprintln!("Error serving connection: {e}");
            }
        });
    }
}

async fn handle(request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let (parts, body) = request.into_parts();
    // consume uploads, the content does not matter
    let _ = body.collect().await;

    let response = match (&parts.method, parts.uri.path()) {
        (&Method::PUT, _) => empty(StatusCode::NO_CONTENT),
        (_, "/cached") => empty(StatusCode::NOT_MODIFIED),
        (_, "/collections") => ok(
            "application/json",
            Some("gzip"),
            gzip(br#"{"collections":[{"id":"ndvi","title":"NDVI"}]}"#),
        ),
        (_, "/collections/ndvi") => ok(
            "application/json",
            Some("br"),
            brotli(br#"{"id":"ndvi","extent":{"spatial":{"bbox":[[-180,-90,180,90]]}}}"#),
        ),
        (_, "/conformance") => ok(
            "application/problem+json",
            Some("zstd"),
            ruzstd::encoding::compress_to_vec(
                &br#"{"type":"about:blank","status":404}"#[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        ),
        (_, "/tiles/0/0/0.png") => ok("image/png", None, fake_png()),
        (_, "/coverage.tif") => ok("image/tiff; application=geotiff", None, fake_tiff()),
        // "Grüße aus München" in ISO-8859-1
        (_, "/legacy.txt") => ok(
            "text/plain; charset=ISO-8859-1",
            None,
            b"Gr\xfc\xdfe aus M\xfcnchen".to_vec(),
        ),
        // `compress` is a registered content coding the proxy cannot decode
        (_, "/old.json") => ok(
            "application/json",
            Some("compress"),
            b"\x1f\x9d\x90{".to_vec(),
        ),
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/problem+json")
            .body(Full::from(r#"{"type":"about:blank","status":404}"#))
            .unwrap_or_default(),
    };

    Ok(response)
}

fn ok(content_type: &str, content_encoding: Option<&str>, body: Vec<u8>) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().header(header::CONTENT_TYPE, content_type);
    if let Some(content_encoding) = content_encoding {
        builder = builder.header(header::CONTENT_ENCODING, content_encoding);
    }
    builder.body(Full::from(body)).unwrap_or_default()
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::default())
        .unwrap_or_default()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(data)
        .and_then(|()| encoder.finish())
        .unwrap_or_default()
}

fn brotli(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 5, 22);
        let _ = writer.write_all(data);
    }
    output
}

/// PNG signature followed by filler bytes; enough for the proxy to see binary data.
fn fake_png() -> Vec<u8> {
    let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
    data.extend(0..=255u8);
    data
}

/// Little-endian TIFF header followed by zeros.
fn fake_tiff() -> Vec<u8> {
    let mut data = b"II*\0".to_vec();
    data.resize(1024, 0);
    data
}
