# Request Logging Proxy

[![CI](https://github.com/geo-engine/request-logging-proxy/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/geo-engine/request-logging-proxy/actions/workflows/ci.yml?branch=main)
[![Crates.io](https://img.shields.io/crates/v/request-logging-proxy.svg)](https://crates.io/crates/request-logging-proxy)

A proxy server that forwards requests and responses and logs them to stdout.

## Building

```bash
cargo build --workspace --all-targets
```

## Running

```bash
cargo run -- --target-url http://localhost:8484
```

This starts the proxy server on port `8123` forwarding requests to `http://localhost:8484`.
It logs requests and responses to stdout in the "vscode" format.
You can customize the target URL, port, and logger format:

```bash
cargo run -- \
    --target-url http://localhost:8484 \
    --port 8123 \
    --logger vscode
```

## Example

```bash
examples/demo.sh
```

This starts a demo upstream server ([examples/demo_upstream.rs](examples/demo_upstream.rs)) with the proxy in front of it and sends requests for compressed JSON (`gzip`, `br`, `zstd`), image tiles, a GeoTIFF up- and download, ISO-8859-1 text, an unsupported content coding, a `HEAD` request and a `304` response.
The log is written to `target/demo/requests.http`, binary bodies to `target/demo/bodies/`.
Pass a directory as the first argument to write somewhere else; `UPSTREAM_PORT` and `PROXY_PORT` change the ports (defaults `18484` and `18123`).

The demo upstream can also be run on its own: `cargo run --example demo_upstream -- 8484`.

## Testing

```bash
cargo test --workspace
```

## Installing

```bash
cargo install --path .
```

```bash
request-logging-proxy --target-url http://localhost:8484
```

## Logger Formats

- `vscode`: Logs requests and responses in a format compatible with VSCode's REST (.http) extension.

Every request/response pair is logged, regardless of its body.
Compressed bodies (`gzip`, `deflate`, `br`, `zstd`) are decompressed for the log only; the client receives the original bytes.
Textual bodies (`text/*`, JSON, XML, `+json`/`+xml` types, …) are decoded by their charset and logged, JSON pretty-printed.
Binary bodies (e.g. `image/png`) are logged as a placeholder comment:

```http
# Body: image/png, 131072 bytes, sha256 3f1c…
```

Use `--max-logged-body-bytes <N>` (or `MAX_LOGGED_BODY_BYTES`) to truncate long textual bodies in the log.

Use `--binary-body-dir <DIR>` (or `BINARY_BODY_DIR`) to store binary bodies as files named `<sha256>.<ext>`.
Identical bodies are stored once.
Request bodies are referenced as `< ./DIR/<file>`, so the logged request can be sent again; response bodies as a `# File:` comment.
Relative paths are written as given, so write the log into the directory you start the proxy from:

```bash
request-logging-proxy --target-url http://localhost:8484 --binary-body-dir bodies > requests.http
```

If you want other formats, feel free to open an issue or a pull request.

## License

This project is licensed under the Apache License Version 2.0.
See the [LICENSE](LICENSE) file for details.
