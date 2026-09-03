#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 077

die() {
  echo "error: $*" >&2
  if [[ -n ${test_root:-} ]]; then
    for log in "$test_root"/logs/*.log; do
      [[ -f $log ]] || continue
      echo "--- $log" >&2
      sed -n '1,160p' "$log" >&2
    done
  fi
  exit 1
}

for command in caddy curl find grep jq mktemp openssl sed sleep tail timeout; do
  command -v "$command" >/dev/null || die "$command is required"
done

case $# in
  0)
    script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
    production_config=$(cd -- "$script_dir/.." && pwd -P)/dev/Caddyfile
    ;;
  1) production_config=$1 ;;
  *) die "usage: scripts/test-dev-gateway.sh [CADDYFILE]" ;;
esac
readonly production_config
[[ -f $production_config ]] || die "Caddyfile does not exist: $production_config"
test_root=$(mktemp -d "${TMPDIR:-/tmp}/maincopy-gateway-test.XXXXXXXX")
readonly test_root
readonly logs="$test_root/logs"
readonly public_upstream_socket="$test_root/public-upstream.sock"
readonly admin_upstream_socket="$test_root/admin-upstream.sock"
readonly gateway_socket="$test_root/gateway.sock"
readonly root_certificate="$test_root/root-ca.pem"
readonly root_private_key="$test_root/root-ca-key.pem"
readonly leaf_certificate="$test_root/gateway.pem"
readonly leaf_private_key="$test_root/gateway-key.pem"
readonly leaf_request="$test_root/gateway.csr"
declare -a child_pids=()

cleanup() {
  local status=$?
  trap - EXIT HUP INT TERM
  local pid
  local watchdog
  local -a watchdog_pids=()
  for pid in "${child_pids[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
      (
        sleep 5
        kill -KILL "$pid" 2>/dev/null || true
      ) &
      watchdog_pids+=("$!")
    fi
  done
  for pid in "${child_pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  for watchdog in "${watchdog_pids[@]}"; do
    kill -TERM "$watchdog" 2>/dev/null || true
    wait "$watchdog" 2>/dev/null || true
  done
  find "$test_root" -depth -delete
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$logs"
export XDG_CONFIG_HOME="$test_root/caddy-config"
export XDG_DATA_HOME="$test_root/caddy-data"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout "$root_private_key" \
  -out "$root_certificate" \
  -days 1 \
  -sha256 \
  -subj "/CN=Maincopy gateway test root" \
  -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" \
  >/dev/null 2>&1
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout "$leaf_private_key" \
  -out "$leaf_request" \
  -sha256 \
  -subj "/CN=admin.localhost" \
  -addext "subjectAltName=DNS:admin.localhost,DNS:maincopy.localhost" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=serverAuth" \
  >/dev/null 2>&1
openssl x509 -req \
  -in "$leaf_request" \
  -CA "$root_certificate" \
  -CAkey "$root_private_key" \
  -CAcreateserial \
  -out "$leaf_certificate" \
  -days 1 \
  -sha256 \
  -copy_extensions copyall \
  >/dev/null 2>&1
openssl verify -CAfile "$root_certificate" "$leaf_certificate" >/dev/null

export MAINCOPY_DEV_TLS_CERTIFICATE="$leaf_certificate"
export MAINCOPY_DEV_TLS_PRIVATE_KEY="$leaf_private_key"
readonly adapted_config="$test_root/adapted-production.json"
readonly gateway_config="$test_root/gateway-test.json"
readonly upstream_config="$test_root/upstreams.json"
if ! caddy adapt --config "$production_config" --adapter caddyfile --pretty \
  >"$adapted_config" 2>"$logs/adapt.log"; then
  die "the production Caddyfile could not be adapted"
fi

jq -e '
  .admin.disabled == true and
  .admin.config.persist == false and
  .apps.http.servers.srv0.listen == ["127.0.0.1:8443", "[::1]:8443"] and
  .apps.http.servers.srv0.strict_sni_host == true and
  .apps.http.servers.srv0.allow_0rtt == false and
  .apps.http.servers.srv0.max_header_bytes == 65536 and
  .apps.http.servers.srv0.protocols == ["h1", "h2"] and
  .apps.http.servers.srv0.automatic_https.disable == true and
  ([
    .apps.http.servers.srv0.routes[]
    | .handle[].routes[]?.handle[]?
    | select(.handler == "reverse_proxy")
  ] | . as $proxies |
    ([$proxies[].upstreams[].dial] | sort) ==
      ["127.0.0.1:3000", "127.0.0.1:3001"] and
    all($proxies[];
      .load_balancing == {} and
      .transport.compression == false and
      .transport.keep_alive.enabled == false and
      .transport.dial_timeout == 2000000000 and
      .transport.response_header_timeout == 30000000000
    )
  )
' "$adapted_config" >/dev/null ||
  die "the production gateway no longer has its fixed listener, upstreams, or bounded transport policy"

jq \
  --arg listener "unix/$gateway_socket" \
  --arg public_upstream "unix/$public_upstream_socket" \
  --arg admin_upstream "unix/$admin_upstream_socket" \
  '
    .apps.http.servers.srv0.listen = [$listener]
    | (..
       | objects
       | select(.dial? == "127.0.0.1:3000")
       | .dial) = $public_upstream
    | (..
       | objects
       | select(.dial? == "127.0.0.1:3001")
       | .dial) = $admin_upstream
  ' "$adapted_config" >"$gateway_config"

jq -e \
  --arg listener "unix/$gateway_socket" \
  --arg public_upstream "unix/$public_upstream_socket" \
  --arg admin_upstream "unix/$admin_upstream_socket" \
  '
    .apps.http.servers.srv0.listen == [$listener] and
    ([
      .apps.http.servers.srv0.routes[]
      | .handle[].routes[]?.handle[]?
      | select(.handler == "reverse_proxy")
      | .upstreams[].dial
    ] | sort) == ([$public_upstream, $admin_upstream] | sort)
  ' "$gateway_config" >/dev/null ||
  die "the temporary gateway transformation did not replace exactly the test endpoints"

jq -n \
  --arg public_listener "unix/$public_upstream_socket" \
  --arg admin_listener "unix/$admin_upstream_socket" \
  '
    def response_body($backend):
      "backend=" + $backend + "\n" +
      "host={http.request.hostport}\n" +
      "origin={http.request.header.Origin}\n" +
      "authorization={http.request.header.Authorization}\n" +
      "cookie={http.request.header.Cookie}\n" +
      "csrf={http.request.header.X-Maincopy-CSRF}\n" +
      "idempotency={http.request.header.Idempotency-Key}\n" +
      "preserved={http.request.header.X-Test-Preserved}\n" +
      "forwarded={http.request.header.Forwarded}\n" +
      "via={http.request.header.Via}\n" +
      "real_ip={http.request.header.X-Real-IP}\n" +
      "x_forwarded_for={http.request.header.X-Forwarded-For}\n" +
      "x_forwarded_host={http.request.header.X-Forwarded-Host}\n" +
      "x_forwarded_proto={http.request.header.X-Forwarded-Proto}\n" +
      "x_forwarded_extra={http.request.header.X-Forwarded-Attacker}\n" +
      "actor={http.request.header.X-Maincopy-Actor}\n" +
      "role={http.request.header.X-Maincopy-Role}\n" +
      "scope={http.request.header.X-Maincopy-Scope}\n";
    def server($listener; $backend): {
      listen: [$listener],
      automatic_https: { disable: true },
      protocols: ["h1"],
      routes: [{
        handle: [{
          handler: "static_response",
          headers: {
            "Content-Type": ["text/plain; charset=utf-8"],
            "Set-Cookie": [
              "first=preserved; Secure; HttpOnly; SameSite=Strict",
              "second=preserved; Secure; HttpOnly; SameSite=Strict"
            ],
            "X-Upstream-Preserved": ["response-preserved"]
          },
          body: response_body($backend)
        }]
      }]
    };
    {
      admin: { disabled: true, config: { persist: false } },
      apps: {
        http: {
          servers: {
            public: server($public_listener; "public"),
            admin: server($admin_listener; "admin")
          }
        }
      }
    }
  ' >"$upstream_config"

caddy validate --config "$upstream_config" >"$logs/upstream-validate.log" 2>&1 ||
  die "the temporary upstream configuration is invalid"
caddy validate --config "$gateway_config" >"$logs/gateway-validate.log" 2>&1 ||
  die "the transformed production gateway configuration is invalid"

start_caddy() {
  local name=$1
  local config=$2
  local data_root="$test_root/$name-data"
  local config_root="$test_root/$name-config"
  local log="$logs/$name.log"
  mkdir -p "$data_root" "$config_root"

  : >"$log"

  XDG_DATA_HOME="$data_root" XDG_CONFIG_HOME="$config_root" \
    caddy run --config "$config" >"$log" 2>&1 &
  local caddy_pid=$!
  child_pids+=("$caddy_pid")
  # The single-quoted program receives the Caddy PID and log as sh positional arguments.
  # shellcheck disable=SC2016
  if ! timeout 10s sh -c '
    tail --pid="$1" -n +1 -f "$2" | grep -Fqm1 "serving initial configuration"
  ' sh "$caddy_pid" "$log"; then
    die "$name did not report readiness within its deadline"
  fi
  kill -0 "$caddy_pid" 2>/dev/null || die "$name exited immediately after reporting readiness"
}

start_caddy upstream "$upstream_config"
start_caddy gateway "$gateway_config"

gateway_curl() {
  local host=$1
  local path=$2
  shift 2
  curl \
    --disable \
    --silent \
    --show-error \
    --http1.1 \
    --max-time 5 \
    --cacert "$root_certificate" \
    --noproxy '*' \
    --proxy '' \
    --unix-socket "$gateway_socket" \
    "$@" \
    "https://$host:8443$path"
}

assert_status() {
  local expected=$1
  local host=$2
  local path=$3
  local output="$test_root/status-body"
  local actual
  actual=$(gateway_curl "$host" "$path" --output "$output" --write-out '%{http_code}')
  [[ $actual == "$expected" ]] ||
    die "$host$path returned HTTP $actual instead of HTTP $expected"
}

for path in /admin /admin/panel /api/admin /api/admin/v1/posts /metrics /metrics/process; do
  assert_status 404 maincopy.localhost "$path"
done
for path in /metrics /metrics/process; do
  assert_status 404 admin.localhost "$path"
done
for path in /adminish /api/adminish /metricsish; do
  assert_status 200 maincopy.localhost "$path"
done

request_backend() {
  local host=$1
  local path=$2
  gateway_curl "$host" "$path" \
    --fail-with-body \
    --header "Host: $host:8443" \
    --header "Origin: https://$host:8443" \
    --header "Authorization: Bearer preserved-authorization" \
    --header "Cookie: session=preserved-cookie" \
    --header "X-Maincopy-CSRF: preserved-csrf" \
    --header "Idempotency-Key: preserved-idempotency" \
    --header "X-Test-Preserved: preserved-custom" \
    --header "Forwarded: spoofed-forwarded" \
    --header "Via: spoofed-via" \
    --header "X-Real-IP: spoofed-real-ip" \
    --header "X-Forwarded-For: spoofed-forwarded-for" \
    --header "X-Forwarded-Host: spoofed-forwarded-host" \
    --header "X-Forwarded-Proto: spoofed-forwarded-proto" \
    --header "X-Forwarded-Attacker: spoofed-forwarded-extra" \
    --header "X-Maincopy-Actor: spoofed-actor" \
    --header "X-Maincopy-Role: spoofed-role" \
    --header "X-Maincopy-Scope: spoofed-scope"
}

assert_line() {
  local body=$1
  local expected=$2
  if ! grep -Fqx -- "$expected" <<<"$body"; then
    echo "upstream response body:" >&2
    printf '%s\n' "$body" >&2
    die "upstream response omitted: $expected"
  fi
}

assert_backend_boundary() {
  local host=$1
  local path=$2
  local backend=$3
  local body
  body=$(request_backend "$host" "$path")
  assert_line "$body" "backend=$backend"
  assert_line "$body" "host=$host:8443"
  assert_line "$body" "origin=https://$host:8443"
  assert_line "$body" "authorization=Bearer preserved-authorization"
  assert_line "$body" "cookie=session=preserved-cookie"
  assert_line "$body" "csrf=preserved-csrf"
  assert_line "$body" "idempotency=preserved-idempotency"
  assert_line "$body" "preserved=preserved-custom"
  if grep -Fq 'spoofed-' <<<"$body"; then
    die "$backend upstream received a spoofed trust-boundary header"
  fi
  assert_line "$body" "forwarded="
  assert_line "$body" "via="
  assert_line "$body" "real_ip="
  assert_line "$body" "x_forwarded_for="
  assert_line "$body" "x_forwarded_host="
  assert_line "$body" "x_forwarded_proto="
  assert_line "$body" "x_forwarded_extra="
  assert_line "$body" "actor="
  assert_line "$body" "role="
  assert_line "$body" "scope="
}

assert_backend_boundary maincopy.localhost /public-boundary public
assert_backend_boundary admin.localhost /api/admin/v1/boundary admin

response_headers="$test_root/response-headers"
gateway_curl admin.localhost /api/admin/v1/response-headers \
  --fail-with-body \
  --dump-header "$response_headers" \
  --output /dev/null
[[ $(grep -Fic 'Set-Cookie:' "$response_headers") == 2 ]] ||
  die "the gateway did not preserve both upstream Set-Cookie headers"
grep -Fiq 'X-Upstream-Preserved: response-preserved' "$response_headers" ||
  die "the gateway did not preserve an ordinary upstream response header"

strict_sni_body="$test_root/strict-sni-body"
strict_sni_status=$(gateway_curl admin.localhost /api/admin/v1/boundary \
  --header 'Host: maincopy.localhost:8443' \
  --output "$strict_sni_body" \
  --write-out '%{http_code}')
[[ $strict_sni_status == 421 ]] ||
  die "mismatched TLS SNI and HTTP Host returned HTTP $strict_sni_status instead of HTTP 421"
if grep -Fq 'backend=' "$strict_sni_body"; then
  die "mismatched TLS SNI and HTTP Host reached an upstream"
fi

reverse_sni_body="$test_root/reverse-sni-body"
reverse_sni_status=$(gateway_curl maincopy.localhost /public-boundary \
  --header 'Host: admin.localhost:8443' \
  --output "$reverse_sni_body" \
  --write-out '%{http_code}')
[[ $reverse_sni_status == 421 ]] ||
  die "reverse-mismatched TLS SNI and HTTP Host returned HTTP $reverse_sni_status instead of HTTP 421"
if grep -Fq 'backend=' "$reverse_sni_body"; then
  die "reverse-mismatched TLS SNI and HTTP Host reached an upstream"
fi

echo "development gateway trust-boundary test passed"
