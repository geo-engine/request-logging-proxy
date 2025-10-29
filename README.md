[![CI](https://github.com/geo-engine/request-logging-proxy/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/geo-engine/request-logging-proxy/actions/workflows/ci.yml?branch=main)

# Request Logging Proxy

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

If you want other formats, feel free to open an issue or a pull request.

## License

This project is licensed under the Apache License Version 2.0.
See the [LICENSE](LICENSE) file for details.
