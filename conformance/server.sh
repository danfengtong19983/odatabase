# Sourced by the client scripts: builds and starts a fresh server, and stops it on exit.
# Containers reach the host through the bridge on Linux, so CI sets BIND=0.0.0.0.
cd "$(dirname "$0")/.."
PORT=${PORT:-38920}
cargo build -q -p odatabase --example conformance_server || exit 1
ODATA_ADDR=${BIND:-127.0.0.1}:$PORT target/debug/examples/conformance_server >/dev/null 2>&1 &
SERVER=$!
trap 'kill $SERVER' EXIT
for _ in 1 2 3 4 5 6 7 8 9 10; do
  curl -sf -o /dev/null "http://127.0.0.1:$PORT/odata/\$metadata" && break
  sleep 1
done
ROOT=http://host.docker.internal:$PORT/odata
