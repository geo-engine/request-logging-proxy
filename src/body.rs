//! Turns raw HTTP message bodies into something that can be written to a log.
//!
//! The functions here never fail: anything that cannot be shown as text
//! (unknown content coding, binary media type, undecodable charset, …)
//! becomes a [`LoggedBody::Binary`] placeholder.

use http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use http::{HeaderMap, Method, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Upper bound for decompressed bodies, to guard against decompression bombs.
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;

/// A body prepared for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoggedBody {
    /// The body is empty or the message cannot have a body.
    None,
    /// A textual body, decoded to UTF-8 and possibly truncated.
    Text {
        text: String,
        truncated_bytes: usize,
    },
    /// A body that is not logged verbatim.
    Binary {
        content_type: Option<String>,
        len: usize,
        sha256: String,
    },
}

impl LoggedBody {
    /// The one-line description of a binary body, e.g.
    /// `image/png, 1234 bytes, sha256 3f1c…`.
    pub fn binary_summary(content_type: Option<&str>, len: usize, sha256: &str) -> String {
        match content_type {
            Some(content_type) => format!("{content_type}, {len} bytes, sha256 {sha256}"),
            None => format!("{len} bytes, sha256 {sha256}"),
        }
    }
}

/// Whether a response to `method` with `status` can carry a body (RFC 9110 §6.4.1).
pub fn response_expects_body(method: &Method, status: StatusCode) -> bool {
    !(method == Method::HEAD
        || status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED)
}

/// Prepare a body for logging.
///
/// `bytes` are the bytes as sent on the wire. They are only decoded for the log;
/// the caller keeps forwarding the original bytes.
pub fn render_body(
    headers: &HeaderMap,
    bytes: &[u8],
    expect_body: bool,
    max_text_bytes: Option<usize>,
) -> LoggedBody {
    if !expect_body || bytes.is_empty() {
        return LoggedBody::None;
    }

    let content_type = headers.get(CONTENT_TYPE).map(|value| match value.to_str() {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => String::from_utf8_lossy(value.as_bytes()),
    });

    let binary = || LoggedBody::Binary {
        content_type: content_type.as_deref().map(str::to_owned),
        len: bytes.len(),
        sha256: sha256_hex(bytes),
    };

    let Some(decoded) = remove_content_encoding(headers, bytes) else {
        return binary();
    };

    let text = match content_type.as_deref() {
        Some(content_type) => {
            let media_type = MediaType::parse(content_type);
            if !media_type.is_text() {
                return binary();
            }
            match decode_charset(&decoded, media_type.charset.as_deref()) {
                Some(text) => text,
                None => return binary(),
            }
        }
        None => match String::from_utf8(decoded.into_owned()) {
            Ok(text) => text,
            Err(_) => return binary(),
        },
    };

    let text = pretty_print_json(text);
    let (text, truncated_bytes) = truncate(text, max_text_bytes);

    LoggedBody::Text {
        text,
        truncated_bytes,
    }
}

/// Store a binary body in `dir` as `<sha256>.<ext>` and return the path of the file.
///
/// Files are content-addressed, so a body that was already stored is not written again.
/// The bytes are stored as received, i.e., with any content coding still applied.
pub async fn store_binary(
    dir: &Path,
    bytes: &[u8],
    content_type: Option<&str>,
    sha256: &str,
) -> std::io::Result<PathBuf> {
    let extension = content_type.map_or("bin", |content_type| {
        let media_type = MediaType::parse(content_type);
        file_extension(&media_type.type_, &media_type.subtype)
    });
    let path = dir.join(format!("{sha256}.{extension}"));
    if !tokio::fs::try_exists(&path).await? {
        tokio::fs::write(&path, bytes).await?;
    }
    Ok(path)
}

/// A file extension for a media type, so that stored bodies open in the right application.
fn file_extension(type_: &str, subtype: &str) -> &'static str {
    match (type_, subtype) {
        ("image", "png") => "png",
        ("image", "jpeg") => "jpg",
        ("image", "gif") => "gif",
        ("image", "webp") => "webp",
        ("image", "tiff") => "tif",
        ("image", "avif") => "avif",
        ("application", "pdf") => "pdf",
        ("application", "zip") => "zip",
        ("application", "gzip") => "gz",
        ("application", "vnd.mapbox-vector-tile") => "mvt",
        ("application", "x-protobuf" | "vnd.google.protobuf") => "pbf",
        ("application", "vnd.apache.parquet") => "parquet",
        ("application", "vnd.apache.arrow.file") => "arrow",
        ("application", "geopackage+sqlite3") => "gpkg",
        ("application", "netcdf" | "x-netcdf") => "nc",
        _ => "bin",
    }
}

/// Undo all content codings (RFC 9110 §8.4).
/// Returns `None` if a coding is unknown or decoding fails.
fn remove_content_encoding<'b>(headers: &HeaderMap, bytes: &'b [u8]) -> Option<Cow<'b, [u8]>> {
    let mut codings = Vec::new();
    for value in headers.get_all(CONTENT_ENCODING) {
        let value = value.to_str().ok()?;
        codings.extend(
            value
                .split(',')
                .map(|coding| coding.trim().to_ascii_lowercase())
                .filter(|coding| !coding.is_empty()),
        );
    }

    let mut data = Cow::Borrowed(bytes);
    // codings are listed in the order they were applied
    for coding in codings.iter().rev() {
        data = match coding.as_str() {
            "identity" => data,
            "gzip" | "x-gzip" => Cow::Owned(read_limited(flate2::read::GzDecoder::new(&*data))?),
            "deflate" => Cow::Owned(
                // `deflate` is the zlib format, but some servers send raw deflate data
                read_limited(flate2::read::ZlibDecoder::new(&*data))
                    .or_else(|| read_limited(flate2::read::DeflateDecoder::new(&*data)))?,
            ),
            "br" => Cow::Owned(read_limited(brotli::Decompressor::new(&*data, 4096))?),
            "zstd" => Cow::Owned(read_limited(
                ruzstd::decoding::StreamingDecoder::new(&*data).ok()?,
            )?),
            _ => return None,
        };
    }

    Some(data)
}

/// Read everything from `reader`, failing if it exceeds [`MAX_DECODED_BYTES`].
fn read_limited<R: Read>(reader: R) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take(MAX_DECODED_BYTES + 1)
        .read_to_end(&mut output)
        .ok()?;
    if output.len() as u64 > MAX_DECODED_BYTES {
        return None;
    }
    Some(output)
}

/// The parts of a `Content-Type` value (RFC 9110 §8.3) that are relevant for logging.
#[derive(Debug, PartialEq, Eq)]
struct MediaType {
    /// lower-case `type`
    type_: String,
    /// lower-case `subtype`
    subtype: String,
    charset: Option<String>,
}

impl MediaType {
    fn parse(value: &str) -> Self {
        let mut parts = value.split(';');
        let essence = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let (type_, subtype) = essence
            .split_once('/')
            .map(|(t, s)| (t.trim().to_owned(), s.trim().to_owned()))
            .unwrap_or((essence.clone(), String::new()));

        let charset = parts.find_map(|param| {
            let (name, value) = param.split_once('=')?;
            if !name.trim().eq_ignore_ascii_case("charset") {
                return None;
            }
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            Some(value.to_owned())
        });

        Self {
            type_,
            subtype,
            charset,
        }
    }

    fn is_text(&self) -> bool {
        if self.type_ == "text" {
            return true;
        }
        if self.type_ != "application" {
            return false;
        }
        matches!(
            self.subtype.as_str(),
            "json" | "xml" | "x-www-form-urlencoded" | "javascript"
        ) || self.subtype.ends_with("+json")
            || self.subtype.ends_with("+xml")
    }
}

/// Decode `bytes` with the given charset, defaulting to UTF-8 (RFC 8259 §8.1, RFC 6657).
fn decode_charset(bytes: &[u8], charset: Option<&str>) -> Option<String> {
    let encoding = match charset {
        Some(label) => encoding_rs::Encoding::for_label(label.as_bytes())?,
        None => encoding_rs::UTF_8,
    };
    encoding
        .decode_without_bom_handling_and_without_replacement(bytes)
        .map(Cow::into_owned)
}

fn pretty_print_json(text: String) -> String {
    serde_json::from_str::<Value>(&text)
        .and_then(|json| serde_json::to_string_pretty(&json))
        .unwrap_or(text)
}

/// Cut `text` to at most `max_bytes` on a char boundary.
/// Returns the text and the number of bytes that were cut off.
fn truncate(mut text: String, max_bytes: Option<usize>) -> (String, usize) {
    let Some(max_bytes) = max_bytes else {
        return (text, 0);
    };
    if text.len() <= max_bytes {
        return (text, 0);
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let truncated_bytes = text.len() - cut;
    text.truncate(cut);
    (text, truncated_bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use std::io::Write;

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const JSON: &[u8] = br#"{"a":1}"#;
    const PRETTY_JSON: &str = "{\n  \"a\": 1\n}";

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    fn text(text: &str) -> LoggedBody {
        LoggedBody::Text {
            text: text.to_owned(),
            truncated_bytes: 0,
        }
    }

    fn render(pairs: &[(&'static str, &'static str)], bytes: &[u8]) -> LoggedBody {
        render_body(&headers(pairs), bytes, true, None)
    }

    #[test]
    fn png_is_binary() {
        assert_eq!(
            render(&[("content-type", "image/png")], PNG_BYTES),
            LoggedBody::Binary {
                content_type: Some("image/png".to_owned()),
                len: PNG_BYTES.len(),
                sha256: sha256_hex(PNG_BYTES),
            }
        );
    }

    #[test]
    fn sha256_is_lower_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn binary_keeps_content_type_parameters() {
        let LoggedBody::Binary { content_type, .. } = render(
            &[("content-type", "image/tiff; application=geotiff")],
            b"II*\0",
        ) else {
            panic!("expected binary");
        };
        assert_eq!(
            content_type.as_deref(),
            Some("image/tiff; application=geotiff")
        );
    }

    #[test]
    fn json_suffix_is_text_case_insensitive() {
        assert_eq!(
            render(
                &[("content-type", "Application/Problem+JSON; charset=utf-8")],
                JSON
            ),
            text(PRETTY_JSON)
        );
        assert_eq!(
            render(&[("content-type", "application/gml+xml")], b"<a/>"),
            text("<a/>")
        );
        assert_eq!(
            render(&[("content-type", "TEXT/HTML")], b"<p>"),
            text("<p>")
        );
    }

    #[test]
    fn decodes_charset() {
        assert_eq!(
            render(
                &[("content-type", "text/plain; charset=\"ISO-8859-1\"")],
                b"caf\xe9"
            ),
            text("café")
        );
    }

    #[test]
    fn invalid_utf8_text_is_binary() {
        assert!(matches!(
            render(&[("content-type", "text/plain")], b"caf\xe9"),
            LoggedBody::Binary { .. }
        ));
        assert!(matches!(
            render(&[("content-type", "text/plain; charset=bogus")], b"x"),
            LoggedBody::Binary { .. }
        ));
    }

    #[test]
    fn without_content_type_sniffs_utf8() {
        assert_eq!(render(&[], b"hello"), text("hello"));
        assert_eq!(
            render(&[], PNG_BYTES),
            LoggedBody::Binary {
                content_type: None,
                len: PNG_BYTES.len(),
                sha256: sha256_hex(PNG_BYTES),
            }
        );
    }

    #[test]
    fn decodes_content_encodings() {
        let json = [("content-type", "application/json")];

        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(JSON).unwrap();
        let gzip = gzip.finish().unwrap();

        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        zlib.write_all(JSON).unwrap();
        let zlib = zlib.finish().unwrap();

        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        raw.write_all(JSON).unwrap();
        let raw = raw.finish().unwrap();

        let mut br = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut br, 4096, 5, 22);
            writer.write_all(JSON).unwrap();
        }

        let zstd =
            ruzstd::encoding::compress_to_vec(JSON, ruzstd::encoding::CompressionLevel::Fastest);

        for (coding, bytes) in [
            ("gzip", &gzip),
            ("x-gzip", &gzip),
            ("deflate", &zlib),
            ("deflate", &raw),
            ("br", &br),
            ("zstd", &zstd),
        ] {
            let mut map = headers(&json);
            map.insert(CONTENT_ENCODING, HeaderValue::from_static(coding));
            assert_eq!(
                render_body(&map, bytes, true, None),
                text(PRETTY_JSON),
                "{coding}"
            );
        }

        // stacked codings are removed in reverse order
        let mut gzip_br = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut gzip_br, 4096, 5, 22);
            writer.write_all(&gzip).unwrap();
        }
        assert_eq!(
            render(
                &[
                    ("content-type", "application/json"),
                    ("content-encoding", "gzip, identity"),
                    ("content-encoding", "br"),
                ],
                &gzip_br
            ),
            text(PRETTY_JSON)
        );
    }

    #[test]
    fn unknown_or_broken_encoding_is_binary() {
        let bytes = b"not compressed";
        assert_eq!(
            render(
                &[
                    ("content-type", "application/json"),
                    ("content-encoding", "compress")
                ],
                bytes
            ),
            LoggedBody::Binary {
                content_type: Some("application/json".to_owned()),
                len: bytes.len(),
                sha256: sha256_hex(bytes),
            }
        );
        assert!(matches!(
            render(
                &[("content-type", "text/plain"), ("content-encoding", "gzip")],
                bytes
            ),
            LoggedBody::Binary { .. }
        ));
    }

    #[test]
    fn empty_or_unexpected_body_is_none() {
        assert_eq!(render(&[], b""), LoggedBody::None);
        assert_eq!(
            render_body(&headers(&[]), b"x", false, None),
            LoggedBody::None
        );
    }

    #[test]
    fn response_body_expectation() {
        assert!(!response_expects_body(&Method::HEAD, StatusCode::OK));
        assert!(!response_expects_body(&Method::GET, StatusCode::CONTINUE));
        assert!(!response_expects_body(&Method::GET, StatusCode::NO_CONTENT));
        assert!(!response_expects_body(
            &Method::GET,
            StatusCode::NOT_MODIFIED
        ));
        assert!(response_expects_body(&Method::GET, StatusCode::OK));
        assert!(response_expects_body(&Method::POST, StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn stores_binary_bodies_content_addressed() {
        let dir = std::env::temp_dir().join(format!("rlp-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let sha = sha256_hex(PNG_BYTES);

        let path = store_binary(&dir, PNG_BYTES, Some("Image/PNG; foo=bar"), &sha)
            .await
            .unwrap();
        assert_eq!(path, dir.join(format!("{sha}.png")));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), PNG_BYTES);

        // storing again is a no-op and yields the same path
        let again = store_binary(&dir, PNG_BYTES, Some("image/png"), &sha)
            .await
            .unwrap();
        assert_eq!(again, path);

        let unknown = store_binary(&dir, b"x", None, "abc").await.unwrap();
        assert_eq!(unknown, dir.join("abc.bin"));

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    #[test]
    fn truncates_on_char_boundary() {
        // "é" is two bytes, so a limit of 2 must not split it
        assert_eq!(
            render_body(
                &headers(&[("content-type", "text/plain")]),
                "aébc".as_bytes(),
                true,
                Some(2)
            ),
            LoggedBody::Text {
                text: "a".to_owned(),
                truncated_bytes: 4,
            }
        );
        assert_eq!(
            render_body(&headers(&[]), b"abc", true, Some(3)),
            text("abc")
        );
    }
}
