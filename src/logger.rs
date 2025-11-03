use anyhow::Context;
use bytes::Bytes;
use flate2::read::GzDecoder;
use http::HeaderValue;
use http::header::CONTENT_ENCODING;
use http_body_util::{BodyExt, Full};
use serde_json::Value;
use std::io::{self, BufWriter};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug)]
pub struct LogEntry {
    pub request: hyper::Request<Full<Bytes>>,
    pub response: hyper::Response<Full<Bytes>>,
}

#[async_trait::async_trait]
pub trait RequestResponseLogger: Send + std::fmt::Debug {
    async fn log_request_response(&mut self, entry: LogEntry) -> anyhow::Result<()>;
}

/// A logger that outputs request and response details to VSCode's REST log format.
#[derive(Debug)]
pub struct VSCodeRestLogger {
    writer: LockableWriter,
}

#[async_trait::async_trait]
impl RequestResponseLogger for VSCodeRestLogger {
    async fn log_request_response(
        &mut self,
        LogEntry { request, response }: LogEntry,
    ) -> anyhow::Result<()> {
        let (request_header, request_body) = request.into_parts();
        let request_body = body_to_string(
            request_body,
            ContentDecoder::try_from(request_header.headers.get(CONTENT_ENCODING))?,
        )
        .await
        .context("Failed to decode request body")?;
        let (response_header, response_body) = response.into_parts();
        let response_body = body_to_string(
            response_body,
            ContentDecoder::try_from(response_header.headers.get(CONTENT_ENCODING))?,
        )
        .await
        .context("failed to decode response body")?;
        let mut w = self.writer.lock()?;
        Self::log_request(&mut w, request_header, request_body)?;
        Self::log_response(&mut w, response_header, response_body)?;
        w.flush()?;

        Ok(())
    }
}

impl VSCodeRestLogger {
    pub fn from_stdout(stdout: io::Stdout) -> Self {
        Self {
            writer: LockableWriter::Stdout(stdout),
        }
    }

    #[cfg(test)]
    pub fn from_buf_writer(buf_writer: Arc<Mutex<BufWriter<Vec<u8>>>>) -> Self {
        Self {
            writer: LockableWriter::BufWriter(buf_writer),
        }
    }

    fn log_request<W: Write>(
        writer: &mut W,
        header: http::request::Parts,
        body: String,
    ) -> anyhow::Result<()> {
        writeln!(
            writer,
            "\n\
            ###\n\
            \n\
            {method} {uri}",
            method = header.method,
            uri = header.uri
        )?;
        for (header, value) in header.headers.iter() {
            writeln!(
                writer,
                "{}: {}",
                header,
                value.to_str().unwrap_or("<invalid UTF-8>")
            )?;
        }

        if !body.is_empty() {
            let body_string = serde_json::from_str::<Value>(&body)
                .and_then(|json| serde_json::to_string_pretty(&json))
                .unwrap_or(body);

            writeln!(writer, "\n{body_string}")?;
        }

        Ok(())
    }

    fn log_response<W: Write>(
        writer: &mut W,
        header: http::response::Parts,
        body: String,
    ) -> anyhow::Result<()> {
        writeln!(
            writer,
            "\n\
            ### Response ###\n\
            #\n\
            # Status Code: {status_code}\n\
            # Response Headers:",
            status_code = header.status
        )?;
        for (header, value) in header.headers.iter() {
            writeln!(
                writer,
                "#   {key}: {value}",
                key = header,
                value = value.to_str().unwrap_or("<invalid UTF-8>")
            )?;
        }
        writeln!(writer, "#")?;

        if !body.is_empty() {
            let body_string = serde_json::from_str::<Value>(&body)
                .and_then(|json| serde_json::to_string_pretty(&json))
                .unwrap_or(body)
                .replace("\n", "\n# "); // Prefix each line with a comment
            writeln!(
                writer,
                "# Body:\n\
                # {body_string}\n\
                #"
            )?;
        }

        Ok(())
    }
}

async fn body_to_string(
    body: Full<Bytes>,
    content_encoding: ContentDecoder,
) -> anyhow::Result<String> {
    let collected = body
        .collect()
        .await
        .map_err(|e| io::Error::other(format!("Failed to collect body: {e}")))?;
    let bytes = collected.to_bytes();

    content_encoding
        .decode(&bytes)
        .with_context(|| format!("Failed to decode body: {}", String::from_utf8_lossy(&bytes)))
}

enum ContentDecoder {
    None,
    Gzip,
}

impl TryFrom<Option<&HeaderValue>> for ContentDecoder {
    type Error = anyhow::Error;

    fn try_from(value: Option<&HeaderValue>) -> Result<Self, Self::Error> {
        match value {
            Some(header_value) => match header_value.to_str() {
                Ok("gzip") => Ok(ContentDecoder::Gzip),
                _ => anyhow::bail!("Unsupported content encoding: {header_value:?}"),
            },
            None => Ok(ContentDecoder::None),
        }
    }
}

impl ContentDecoder {
    fn decode(&self, data: &[u8]) -> anyhow::Result<String> {
        if data.is_empty() {
            return Ok(String::new());
        }

        match self {
            ContentDecoder::None => {
                String::from_utf8(data.to_vec()).context("Failed to convert body to string")
            }
            ContentDecoder::Gzip => {
                let mut decoder = GzDecoder::new(data);
                let mut decoded_data = String::new();
                decoder
                    .read_to_string(&mut decoded_data)
                    .context("Failed to decode gzip data")?;
                Ok(decoded_data)
            }
        }
    }
}

#[derive(Debug)]
pub enum LockableWriter {
    Stdout(io::Stdout),
    #[allow(dead_code)] // for tests
    BufWriter(Arc<Mutex<BufWriter<Vec<u8>>>>),
}

impl LockableWriter {
    pub fn lock<'w>(&'w mut self) -> anyhow::Result<LockableWriterGuard<'w>> {
        match self {
            LockableWriter::Stdout(stdout) => Ok(LockableWriterGuard::Stdout(stdout.lock())),
            LockableWriter::BufWriter(mutex) => {
                Ok(LockableWriterGuard::BufWriter(mutex.lock().map_err(
                    |e| io::Error::other(format!("Failed to lock writer: {e}")),
                )?))
            }
        }
    }
}

#[derive(Debug)]
pub enum LockableWriterGuard<'w> {
    Stdout(io::StdoutLock<'w>),
    BufWriter(MutexGuard<'w, BufWriter<Vec<u8>>>),
}

impl Write for LockableWriterGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            LockableWriterGuard::Stdout(lock) => lock.write(buf),
            LockableWriterGuard::BufWriter(guard) => guard.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            LockableWriterGuard::Stdout(lock) => lock.flush(),
            LockableWriterGuard::BufWriter(guard) => guard.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn it_logs_requests_and_responses() {
        let buf_writer = Arc::new(Mutex::new(BufWriter::new(Vec::new())));
        let mut logger = VSCodeRestLogger::from_buf_writer(buf_writer.clone());

        let request = hyper::Request::builder()
            .method("GET")
            .uri("http://example.com/test")
            .header("Content-Type", "application/json")
            .body(Full::from(Bytes::from_static(b"{\"key\":\"value\"}")))
            .unwrap();

        let response = hyper::Response::builder()
            .status(200)
            .header("Content-Type", "application/json")
            .body(Full::from(Bytes::from_static(
                b"{\"response_key\":\"response_value\"}",
            )))
            .unwrap();

        logger
            .log_request_response(LogEntry { request, response })
            .await
            .unwrap();

        let logged_output = buf_writer.lock().unwrap().get_ref().clone();
        let logged_string = String::from_utf8(logged_output).unwrap();

        assert_eq!(logged_string, include_str!("../test-data/logged.http"));
    }
}
