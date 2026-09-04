#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

usage() {
  cat <<'EOF'
Usage: scripts/dev.sh [--trust-browser]

Build and run the example Maincopy server plus its loopback HTTPS gateway.
On fresh state, the daemon prints a generated owner password exactly once.
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

gateway_arguments=()
case ${1:-} in
  "") ;;
  --trust-browser) gateway_arguments+=(--trust-browser) ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
(($# <= 1)) || die "too many arguments"

for command in caddy cargo curl flock mkcert; do
  command -v "$command" >/dev/null || die "$command is required; run this inside nix develop"
done

script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
readonly script_dir
project_root=$(cd -- "$script_dir/.." && pwd -P)
readonly project_root
readonly config="$project_root/crates/server/examples/development/maincopy.toml"
readonly daemon="$project_root/target/debug/maincopyd"
if [[ -n ${XDG_DATA_HOME:-} ]]; then
  maincopy_data_root=$XDG_DATA_HOME
elif [[ -n ${HOME:-} ]]; then
  maincopy_data_root="$HOME/.local/share"
else
  die "neither XDG_DATA_HOME nor HOME identifies durable user state"
fi
[[ $maincopy_data_root == /* ]] || die "the development data root must be an absolute path"
readonly maincopy_data_root
readonly root_certificate="$maincopy_data_root/maincopy/dev-ca/rootCA.pem"

cd -- "$project_root"
cargo build --locked \
  --package maincopy-server --bin maincopyd \
  --package maincopy-diagram-renderer --bin maincopy-mermaid \
  --package maincopy-cli --bin maincopy

daemon_pid=
gateway_pid=
readonly readiness_timeout_seconds=30
# shellcheck disable=SC2329 # Invoked indirectly by the EXIT trap.
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  [[ -z $gateway_pid ]] || kill -TERM "$gateway_pid" 2>/dev/null || true
  [[ -z $daemon_pid ]] || kill -TERM "$daemon_pid" 2>/dev/null || true
  [[ -z $gateway_pid ]] || wait "$gateway_pid" 2>/dev/null || true
  [[ -z $daemon_pid ]] || wait "$daemon_pid" 2>/dev/null || true
  exit "$status"
}
trap cleanup EXIT
# Preserve the conventional interrupt status; Justfile suppresses its exit message.
trap 'exit 130' INT
trap 'exit 143' TERM

die_if_exited() {
  local name=$1
  local pid=$2
  local status=0
  kill -0 "$pid" 2>/dev/null && return
  wait "$pid" || status=$?
  die "$name exited before becoming ready (status $status)"
}

"$daemon" --config "$config" &
daemon_pid=$!

daemon_ready=false
daemon_deadline=$((SECONDS + readiness_timeout_seconds))
while ((SECONDS < daemon_deadline)); do
  if curl --fail --silent \
    --connect-timeout 0.1 \
    --max-time 0.15 \
    --noproxy '*' \
    --header 'Host: maincopy.localhost:8443' \
    http://127.0.0.1:3000/health/ready >/dev/null; then
    daemon_ready=true
    break
  fi
  die_if_exited maincopyd "$daemon_pid"
  sleep 0.1
done
$daemon_ready || die "maincopyd did not become ready within $readiness_timeout_seconds seconds"

"$script_dir/dev-gateway.sh" "${gateway_arguments[@]}" &
gateway_pid=$!

gateway_ready=false
gateway_deadline=$((SECONDS + readiness_timeout_seconds))
while ((SECONDS < gateway_deadline)); do
  if [[ -f $root_certificate ]] && \
    curl --fail --silent \
      --connect-timeout 0.1 \
      --max-time 0.15 \
      --noproxy '*' \
      --cacert "$root_certificate" \
      https://maincopy.localhost:8443/health/ready >/dev/null && \
    curl --fail --silent \
      --connect-timeout 0.1 \
      --max-time 0.15 \
      --noproxy '*' \
      --cacert "$root_certificate" \
      https://admin.localhost:8443/admin/login >/dev/null; then
    gateway_ready=true
    break
  fi
  die_if_exited maincopyd "$daemon_pid"
  die_if_exited gateway "$gateway_pid"
  sleep 0.1
done
$gateway_ready || die "the development gateway did not become ready within $readiness_timeout_seconds seconds"

cat <<'EOF'

Maincopy development environment is ready.

  Public: https://maincopy.localhost:8443
  Admin:  https://admin.localhost:8443/admin/login
  CLI:    scripts/dev-maincopy.sh login --username owner

Use Ctrl+C here to stop the server and gateway.
EOF

set +e
wait -n "$daemon_pid" "$gateway_pid"
child_status=$?
set -e
echo "error: a local development process exited unexpectedly" >&2
((child_status != 0)) || child_status=1
exit "$child_status"
