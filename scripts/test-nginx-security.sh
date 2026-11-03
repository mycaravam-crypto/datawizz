#!/usr/bin/env bash
set -euo pipefail

# CI-only ephemeral TLS certificate. No private keys are committed.
sudo apt-get update
sudo apt-get install -y --no-install-recommends nginx openssl
sudo mkdir -p /etc/nginx/certs
sudo openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -subj '/CN=datawizz.example.com' \
  -keyout /etc/nginx/certs/datawizz.key \
  -out /etc/nginx/certs/datawizz.crt >/dev/null 2>&1
sudo cp deploy/nginx/datawizz.conf /etc/nginx/sites-enabled/datawizz
sudo rm -f /etc/nginx/sites-enabled/default
sudo nginx -t
python3 -m http.server 3000 --bind 127.0.0.1 >/tmp/datawizz-proxy-upstream.log 2>&1 &
upstream_pid=$!
trap 'kill "$upstream_pid" || true; sudo nginx -s stop || true' EXIT
sudo nginx
request() {
  curl --noproxy '*' -ksS --resolve datawizz.example.com:443:127.0.0.1 \
    -o /dev/null -w '%{http_code}' "$@" https://datawizz.example.com/
}
for attempt in $(seq 1 20); do
  if [[ "$(request)" == 200 ]]; then break; fi
  sleep 1
done
[[ "$(request)" == 200 ]]
[[ "$(request -H 'Origin: https://datawizz.example.com')" == 200 ]]
[[ "$(request -H 'Origin: https://untrusted.example.net')" == 403 ]]
headers=$(curl --noproxy '*' -ksSI --resolve datawizz.example.com:443:127.0.0.1 https://datawizz.example.com/)
grep -qi 'strict-transport-security:' <<<"$headers"
grep -qi 'x-content-type-options: nosniff' <<<"$headers"
grep -qi 'cache-control: no-store' <<<"$headers"
echo 'HTTPS proxy configuration, Origin rejection and security headers passed'
