use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use serde_json::Value;
use std::io;
use std::io::Write;

pub struct LogEntry {
    pub request: hyper::Request<Full<Bytes>>,
    pub response: hyper::Response<Full<Bytes>>,
}

#[async_trait::async_trait]
pub trait RequestResponseLogger: Send + std::fmt::Debug {
    async fn log_request_response(&mut self, entry: LogEntry) -> io::Result<()>;
}

/// A logger that outputs request and response details to VSCode's REST log format.
#[derive(Debug)]
pub struct VSCodeRestLogger {
    stdout: io::Stdout,
}

#[async_trait::async_trait]
impl RequestResponseLogger for VSCodeRestLogger {
    async fn log_request_response(
        &mut self,
        LogEntry { request, response }: LogEntry,
    ) -> io::Result<()> {
        let (request_header, request_body) = request.into_parts();
        let request_body = body_to_string(request_body).await?;
        let (response_header, response_body) = response.into_parts();
        let response_body = body_to_string(response_body).await?;

        let mut stdout = self.stdout.lock();
        Self::log_request(&mut stdout, request_header, request_body)?;
        Self::log_response(&mut stdout, response_header, response_body)?;
        stdout.flush()?;

        Ok(())
    }
}

impl VSCodeRestLogger {
    pub fn new(stdout: io::Stdout) -> Self {
        Self { stdout }
    }

    fn log_request(
        stdout: &mut io::StdoutLock<'_>,
        header: http::request::Parts,
        body: String,
    ) -> io::Result<()> {
        writeln!(
            stdout,
            "\n\
            ###\n\
            \n\
            {method} {uri}",
            method = header.method,
            uri = header.uri
        )?;
        for (header, value) in header.headers.iter() {
            writeln!(
                stdout,
                "{}: {}",
                header,
                value.to_str().unwrap_or("<invalid UTF-8>")
            )?;
        }

        if !body.is_empty() {
            let body_string = serde_json::from_str::<Value>(&body)
                .and_then(|json| serde_json::to_string_pretty(&json))
                .unwrap_or(body);

            writeln!(stdout, "\n{body_string}")?;
        }

        Ok(())
    }

    fn log_response(
        stdout: &mut io::StdoutLock<'_>,
        header: http::response::Parts,
        body: String,
    ) -> io::Result<()> {
        writeln!(
            stdout,
            "\n\
            ### Response ###\n\
            #\n\
            # Status Code: {status_code}\n\
            # Response Headers:",
            status_code = header.status
        )?;
        for (header, value) in header.headers.iter() {
            writeln!(
                stdout,
                "#   {key}: {value}",
                key = header,
                value = value.to_str().unwrap_or("<invalid UTF-8>")
            )?;
        }
        writeln!(stdout, "#")?;

        if !body.is_empty() {
            let body_string = serde_json::from_str::<Value>(&body)
                .and_then(|json| serde_json::to_string_pretty(&json))
                .unwrap_or(body)
                .replace("\n", "\n# "); // Prefix each line with a comment
            writeln!(
                stdout,
                "# Body:\n\
                # {body_string}\n\
                #"
            )?;
        }

        Ok(())
    }
}

async fn body_to_string(body: Full<Bytes>) -> io::Result<String> {
    let collected = body
        .collect()
        .await
        .map_err(|e| io::Error::other(format!("Failed to collect body: {e}")))?;
    String::from_utf8(collected.to_bytes().to_vec())
        .map_err(|e| io::Error::other(format!("Failed to convert body to string: {e}")))
}
