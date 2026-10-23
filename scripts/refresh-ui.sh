#!/usr/bin/env bash
set -euo pipefail

# Rebuild the Rust binary (web/index.html is embedded at compile time),
# recreate the application container and verify what the browser receives.
command -v docker >/dev/null || { echo "Docker is required" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is required to verify the served UI" >&2; exit 1; }
command -v cmp >/dev/null || { echo "cmp is required to verify the served UI" >&2; exit 1; }

cd "$(dirname "$0")/.."

docker compose up --detach --build --force-recreate app

url="${DATAWIZZ_URL:-http://127.0.0.1:3000/}"
for attempt in $(seq 1 30); do
  if curl --fail --silent --show-error --max-time 3 "$url" 2>/dev/null | cmp -s web/index.html -; then
    echo "Verified: the running application serves the exact local web/index.html."
    echo "Open $url"
    exit 0
  fi
  sleep 1
done

echo "UI verification failed: the served HTML does not match web/index.html." >&2
echo "Check: docker compose ps && docker compose logs --tail=100 app" >&2
exit 1
