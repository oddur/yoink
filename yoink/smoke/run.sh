#!/usr/bin/env bash
# End-to-end CLI smoke test for `yoink up --no-registry --transport=unregistry`.
#
# Builds a tiny `sleep`-only container locally, pushes it to
# `backtrack-eu-1` via the unregistry transport, asserts the container
# is running on the host, then tears it down.
#
# Usage (from the yoink/yoink/ directory):
#   ./smoke/run.sh
#
# Set YOINK_BIN to override which yoink binary to use (default: built
# from this checkout via `cargo run`).
#
# Exits 0 on success, non-zero with a diagnostic on the first failure.

set -euo pipefail

SMOKE_DIR="$(cd "$(dirname "$0")" && pwd)"
YOINK_BIN="${YOINK_BIN:-cargo run --quiet --manifest-path "$SMOKE_DIR/../Cargo.toml" --}"
HOST_ADDR="backtrack-eu-1"
HOST_USER="root"
SERVICE="yoink-smoke-test"
TAG="smoke-$(date +%s)"

cd "$SMOKE_DIR"

trap 'echo; echo "=== smoke FAILED ==="; cleanup || true' ERR

cleanup() {
  echo "[cleanup] removing containers + image + network from $HOST_ADDR"
  # Remove every container labelled with this service (yoink names
  # them `<service>-<hash>`, can be more than one across replicas /
  # half-rolled deploys).
  ids=$(docker -H "ssh://$HOST_USER@$HOST_ADDR" \
    ps -aq --filter "label=yoink.service=$SERVICE" 2>/dev/null || true)
  if [ -n "$ids" ]; then
    # shellcheck disable=SC2086
    docker -H "ssh://$HOST_USER@$HOST_ADDR" rm -f $ids >/dev/null 2>&1 || true
  fi
  docker -H "ssh://$HOST_USER@$HOST_ADDR" rmi -f "$SERVICE:$TAG" >/dev/null 2>&1 || true
  docker -H "ssh://$HOST_USER@$HOST_ADDR" network rm yoink-smoke-net >/dev/null 2>&1 || true
  docker rmi -f "$SERVICE:$TAG" >/dev/null 2>&1 || true
}

echo "== smoke CLI: yoink up --no-registry --transport=unregistry =="
echo "  service=$SERVICE  tag=$TAG  host=$HOST_USER@$HOST_ADDR"
echo

echo "[1/4] yoink build (--no-cache to make every push exercise the transport)"
$YOINK_BIN build --service "$SERVICE" --tag "$SERVICE=$TAG" --allow-dirty -c "$SMOKE_DIR/yoink.yaml"

echo
echo "[2/4] first deploy via unregistry transport (cold push)"
time $YOINK_BIN up \
  --service "$SERVICE" \
  --tag "$SERVICE=$TAG" \
  --no-registry \
  --transport unregistry \
  --allow-dirty \
  -c "$SMOKE_DIR/yoink.yaml"

echo
echo "[3/4] verify container is running on $HOST_ADDR"
# yoink names containers `<service>-<hash>`; query by service label
# instead of guessing the suffix.
running=$(docker -H "ssh://$HOST_USER@$HOST_ADDR" \
  ps --filter "label=yoink.service=$SERVICE" --filter "status=running" --format '{{.Names}}')
if [ -z "$running" ]; then
  echo "FAIL: no running container with label yoink.service=$SERVICE on $HOST_ADDR"
  docker -H "ssh://$HOST_USER@$HOST_ADDR" ps -a --filter "label=yoink.service=$SERVICE" || true
  exit 1
fi
echo "  ✓ running: $running"

echo
echo "[4/4] re-deploy with same tag (should hit blob dedup, much faster)"
time $YOINK_BIN up \
  --service "$SERVICE" \
  --tag "$SERVICE=$TAG" \
  --no-registry \
  --transport unregistry \
  --allow-dirty \
  -c "$SMOKE_DIR/yoink.yaml"

echo
echo "✓ smoke CLI passed"
echo
cleanup
