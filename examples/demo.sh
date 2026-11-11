#!/usr/bin/env bash
#
# Run the proxy in front of the demo upstream, send a few requests and write the log.
#
#   examples/demo.sh [OUTPUT_DIR]
#
# OUTPUT_DIR defaults to target/demo and contains `requests.http` and `bodies/` afterwards.
# Requires curl.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$(mkdir -p "${1:-$ROOT/target/demo}" && cd "${1:-$ROOT/target/demo}" && pwd)"
UPSTREAM_PORT="${UPSTREAM_PORT:-18484}"
PROXY_PORT="${PROXY_PORT:-18123}"
PROXY="http://localhost:$PROXY_PORT"

cargo build --quiet --manifest-path "$ROOT/Cargo.toml" --bin request-logging-proxy --example demo_upstream

rm -rf "$OUT/requests.http" "$OUT/bodies"

pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; }
trap cleanup EXIT

"$ROOT/target/debug/examples/demo_upstream" "$UPSTREAM_PORT" &
pids+=($!)

# Run from the output directory, so that the relative body paths in the log resolve.
(
    cd "$OUT"
    exec "$ROOT/target/debug/request-logging-proxy" \
        --target-url "http://127.0.0.1:$UPSTREAM_PORT" \
        --port "$PROXY_PORT" \
        --binary-body-dir bodies \
        --max-logged-body-bytes 4096 \
        >requests.http
) &
pids+=($!)

# Wait until both servers accept connections, without sending a request that would be logged.
port_open() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
for _ in $(seq 100); do
    if port_open "$UPSTREAM_PORT" && port_open "$PROXY_PORT"; then break; fi
    sleep 0.1
done

send() {
    echo "  $*" >&2
    curl -s -o /dev/null "$@"
}

echo "Sending requests through $PROXY:" >&2
send "$PROXY/collections"                         # gzip-compressed JSON
send "$PROXY/collections/ndvi"                    # brotli-compressed JSON
send "$PROXY/conformance"                         # zstd-compressed problem+json
send "$PROXY/tiles/0/0/0.png"                     # binary, stored in bodies/
send "$PROXY/tiles/0/0/0.png"                     # same tile, stored only once
echo "  $PROXY/coverage.tif" >&2                 # GeoTIFF with media type parameters
curl -s -o "$OUT/upload.tif" "$PROXY/coverage.tif"
send -X PUT -H 'Content-Type: image/tiff; application=geotiff' \
    --data-binary "@$OUT/upload.tif" "$PROXY/coverage.tif" # binary request body
send "$PROXY/legacy.txt"                          # ISO-8859-1 text
send "$PROXY/old.json"                            # unsupported content coding
send -I "$PROXY/collections"                      # HEAD: no body
send "$PROXY/cached"                              # 304: no body
rm -f "$OUT/upload.tif"

# Give the logger time to write the last entries before shutting down.
sleep 0.5
cleanup
wait 2>/dev/null || true

echo >&2
echo "Log:    $OUT/requests.http" >&2
echo "Bodies: $OUT/bodies/" >&2
