use crate::body::{LoggedBody, render_body, response_expects_body, store_binary};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
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

/// A logger that does not forward outputs requests and responses.
#[derive(Debug)]
pub struct DevNullLogger;

#[async_trait::async_trait]
impl RequestResponseLogger for DevNullLogger {
    async fn log_request_response(
        &mut self,
        LogEntry {
            request: _,
            response: _,
        }: LogEntry,
    ) -> anyhow::Result<()> {
        // Do nothing with the request and response
        Ok(())
    }
}

/// A logger that outputs request and response details to VSCode's REST log format.
#[derive(Debug)]
pub struct VSCodeRestLogger {
    writer: LockableWriter,
    options: VSCodeRestLoggerOptions,
}

#[derive(Debug, Default, Clone)]
pub struct VSCodeRestLoggerOptions {
    /// Truncate logged text bodies after this many bytes.
    pub max_text_body_bytes: Option<usize>,
    /// Store binary bodies in this directory and link them from the log.
    pub binary_body_dir: Option<PathBuf>,
}

/// A body as it is written to the log, with the outcome of storing it to a file.
struct BodyOutput {
    body: LoggedBody,
    file: Option<io::Result<PathBuf>>,
}

#[async_trait::async_trait]
impl RequestResponseLogger for VSCodeRestLogger {
    async fn log_request_response(
        &mut self,
        LogEntry { request, response }: LogEntry,
    ) -> anyhow::Result<()> {
        let (request_header, request_body) = request.into_parts();
        let request_body = self
            .prepare_body(&request_header.headers, collect(request_body).await, true)
            .await;
        let (response_header, response_body) = response.into_parts();
        let response_body = self
            .prepare_body(
                &response_header.headers,
                collect(response_body).await,
                response_expects_body(&request_header.method, response_header.status),
            )
            .await;
        let mut w = self.writer.lock()?;
        Self::log_request(&mut w, request_header, request_body)?;
        Self::log_response(&mut w, response_header, response_body)?;
        w.flush()?;

        Ok(())
    }
}

impl VSCodeRestLogger {
    pub fn from_stdout(stdout: io::Stdout, options: VSCodeRestLoggerOptions) -> Self {
        Self {
            writer: LockableWriter::Stdout(stdout),
            options,
        }
    }

    #[cfg(test)]
    pub fn from_buf_writer(
        buf_writer: Arc<Mutex<BufWriter<Vec<u8>>>>,
        options: VSCodeRestLoggerOptions,
    ) -> Self {
        Self {
            writer: LockableWriter::BufWriter(buf_writer),
            options,
        }
    }

    async fn prepare_body(
        &self,
        headers: &http::HeaderMap,
        bytes: Bytes,
        expect_body: bool,
    ) -> BodyOutput {
        let body = render_body(
            headers,
            &bytes,
            expect_body,
            self.options.max_text_body_bytes,
        );
        let file = match (&body, &self.options.binary_body_dir) {
            (
                LoggedBody::Binary {
                    content_type,
                    sha256,
                    ..
                },
                Some(dir),
            ) => Some(store_binary(dir, &bytes, content_type.as_deref(), sha256).await),
            _ => None,
        };
        BodyOutput { body, file }
    }

    fn log_request<W: Write>(
        writer: &mut W,
        header: http::request::Parts,
        BodyOutput { body, file }: BodyOutput,
    ) -> anyhow::Result<()> {
        writeln!(writer, "\n###\n")?;

        // Comments go before the request line, because inside the body they would be sent as content.
        match &body {
            LoggedBody::Text {
                truncated_bytes, ..
            } if *truncated_bytes > 0 => {
                writeln!(writer, "# Body: {truncated_bytes} more bytes truncated")?;
            }
            LoggedBody::Binary {
                content_type,
                len,
                sha256,
            } => {
                writeln!(
                    writer,
                    "# Body: {}",
                    LoggedBody::binary_summary(content_type.as_deref(), *len, sha256)
                )?;
                if let Some(Err(e)) = &file {
                    writeln!(writer, "# Body could not be stored: {e}")?;
                }
            }
            _ => {}
        }

        writeln!(
            writer,
            "{method} {uri}",
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

        match (body, file) {
            (LoggedBody::Text { text, .. }, _) => writeln!(writer, "\n{text}")?,
            // the request body is read from the file when the request is sent again
            (LoggedBody::Binary { .. }, Some(Ok(path))) => {
                writeln!(writer, "\n< {}", file_reference(&path))?;
            }
            _ => {}
        }

        Ok(())
    }

    fn log_response<W: Write>(
        writer: &mut W,
        header: http::response::Parts,
        BodyOutput { body, file }: BodyOutput,
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

        match body {
            LoggedBody::None => {}
            LoggedBody::Text {
                text,
                truncated_bytes,
            } => {
                let text = text.replace('\n', "\n# "); // Prefix each line with a comment
                writeln!(writer, "# Body:\n# {text}")?;
                if truncated_bytes > 0 {
                    writeln!(writer, "# … {truncated_bytes} more bytes truncated")?;
                }
                writeln!(writer, "#")?;
            }
            LoggedBody::Binary {
                content_type,
                len,
                sha256,
            } => {
                writeln!(
                    writer,
                    "# Body: {}",
                    LoggedBody::binary_summary(content_type.as_deref(), len, &sha256)
                )?;
                match file {
                    Some(Ok(path)) => writeln!(writer, "# File: {}", file_reference(&path))?,
                    Some(Err(e)) => writeln!(writer, "# Body could not be stored: {e}")?,
                    None => {}
                }
                writeln!(writer, "#")?;
            }
        }

        Ok(())
    }
}

/// Format a path for the log. Relative paths get a `./` prefix, as in the HTTP client examples.
fn file_reference(path: &Path) -> String {
    if path.is_relative() && !path.starts_with(".") && !path.starts_with("..") {
        format!("./{}", path.display())
    } else {
        path.display().to_string()
    }
}

/// Collect a fully buffered body. This cannot fail.
async fn collect(body: Full<Bytes>) -> Bytes {
    let Ok(collected) = body.collect().await;
    collected.to_bytes()
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
        let mut logger = VSCodeRestLogger::from_buf_writer(buf_writer.clone(), Default::default());

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

    async fn log(
        request: hyper::Request<Full<Bytes>>,
        response: hyper::Response<Full<Bytes>>,
    ) -> String {
        log_with(Default::default(), request, response).await
    }

    async fn log_with(
        options: VSCodeRestLoggerOptions,
        request: hyper::Request<Full<Bytes>>,
        response: hyper::Response<Full<Bytes>>,
    ) -> String {
        let buf_writer = Arc::new(Mutex::new(BufWriter::new(Vec::new())));
        let mut logger = VSCodeRestLogger::from_buf_writer(buf_writer.clone(), options);
        logger
            .log_request_response(LogEntry { request, response })
            .await
            .unwrap();
        let logged_output = buf_writer.lock().unwrap().get_ref().clone();
        String::from_utf8(logged_output).unwrap()
    }

    #[tokio::test]
    async fn it_logs_binary_responses_as_placeholder() {
        let request = hyper::Request::builder()
            .method("GET")
            .uri("http://example.com/tile.png")
            .body(Full::default())
            .unwrap();
        let response = hyper::Response::builder()
            .status(200)
            .header("Content-Type", "image/png")
            .body(Full::from(Bytes::from_static(b"abc")))
            .unwrap();

        assert_eq!(
            log(request, response).await,
            "\n###\n\nGET http://example.com/tile.png\n\n### Response ###\n#\n# Status Code: 200 OK\n\
             # Response Headers:\n#   content-type: image/png\n#\n\
             # Body: image/png, 3 bytes, sha256 ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\n#\n"
        );
    }

    #[tokio::test]
    async fn it_logs_no_body_for_head_responses() {
        let request = hyper::Request::builder()
            .method("HEAD")
            .uri("http://example.com/test")
            .body(Full::default())
            .unwrap();
        // a body that must be ignored
        let response = hyper::Response::builder()
            .status(200)
            .header("Content-Type", "application/json")
            .body(Full::from(Bytes::from_static(b"{}")))
            .unwrap();

        assert_eq!(
            log(request, response).await,
            "\n###\n\nHEAD http://example.com/test\n\n### Response ###\n#\n# Status Code: 200 OK\n\
             # Response Headers:\n#   content-type: application/json\n#\n"
        );
    }

    #[tokio::test]
    async fn it_stores_binary_bodies_in_files() {
        const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let dir = std::env::temp_dir().join(format!("rlp-logger-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let request = hyper::Request::builder()
            .method("PUT")
            .uri("http://example.com/upload")
            .header("Content-Type", "image/tiff; application=geotiff")
            .body(Full::from(Bytes::from_static(b"abc")))
            .unwrap();
        let response = hyper::Response::builder()
            .status(200)
            .header("Content-Type", "image/png")
            .body(Full::from(Bytes::from_static(b"abc")))
            .unwrap();

        let logged = log_with(
            VSCodeRestLoggerOptions {
                binary_body_dir: Some(dir.clone()),
                ..Default::default()
            },
            request,
            response,
        )
        .await;

        let tif = dir.join(format!("{SHA_ABC}.tif"));
        let png = dir.join(format!("{SHA_ABC}.png"));
        assert_eq!(
            logged,
            format!(
                "\n###\n\n\
                 # Body: image/tiff; application=geotiff, 3 bytes, sha256 {SHA_ABC}\n\
                 PUT http://example.com/upload\n\
                 content-type: image/tiff; application=geotiff\n\
                 \n< {tif}\n\
                 \n### Response ###\n#\n# Status Code: 200 OK\n\
                 # Response Headers:\n#   content-type: image/png\n#\n\
                 # Body: image/png, 3 bytes, sha256 {SHA_ABC}\n\
                 # File: {png}\n#\n",
                tif = tif.display(),
                png = png.display(),
            )
        );
        assert_eq!(tokio::fs::read(&tif).await.unwrap(), b"abc");
        assert_eq!(tokio::fs::read(&png).await.unwrap(), b"abc");

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn it_prefixes_relative_file_references() {
        assert_eq!(file_reference(Path::new("bodies/a.png")), "./bodies/a.png");
        assert_eq!(file_reference(Path::new("./a.png")), "./a.png");
        assert_eq!(file_reference(Path::new("../a.png")), "../a.png");
        assert_eq!(file_reference(Path::new("/tmp/a.png")), "/tmp/a.png");
    }
}
