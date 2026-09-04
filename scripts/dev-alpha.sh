#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

usage() {
  cat <<'EOF'
Usage: scripts/dev-alpha.sh [--trust-browser]

Build and run the example Maincopy server plus its loopback HTTPS gateway.
On first use, bootstrap prompts twice for the owner password.
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

for command in cargo curl; do
  command -v "$command" >/dev/null || die "$command is required; run this inside nix develop"
done

script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
readonly script_dir
project_root=$(cd -- "$script_dir/.." && pwd -P)
readonly project_root
readonly config="$project_root/crates/server/examples/local-alpha/maincopy.toml"
readonly daemon="$project_root/target/debug/maincopyd"
readonly database="$project_root/target/maincopy-dev/state/maincopy.db"
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

if [[ ! -e $database ]]; then
  "$daemon" --config "$config" identity bootstrap password --username owner
fi

daemon_pid=
gateway_pid=
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
trap 'exit 130' INT
trap 'exit 143' TERM

"$daemon" --config "$config" &
daemon_pid=$!
"$script_dir/dev-gateway.sh" "${gateway_arguments[@]}" &
gateway_pid=$!

ready=false
for _ in {1..50}; do
  if [[ -f $root_certificate ]] && \
    curl --fail --silent \
      --connect-timeout 0.1 \
      --max-time 0.15 \
      --noproxy '*' \
      --cacert "$root_certificate" \
      https://maincopy.localhost:8443/health/ready >/dev/null; then
    ready=true
    break
  fi
  kill -0 "$daemon_pid" 2>/dev/null || wait "$daemon_pid"
  kill -0 "$gateway_pid" 2>/dev/null || wait "$gateway_pid"
  sleep 0.1
done
$ready || die "the local alpha did not become ready within 15 seconds"

cat <<'EOF'

Maincopy local alpha is ready.

  Public: https://maincopy.localhost:8443
  Login:  scripts/dev-maincopy.sh login --username owner
  Posts:  scripts/dev-maincopy.sh posts

Use Ctrl+C here to stop the server and gateway.
EOF

set +e
wait -n "$daemon_pid" "$gateway_pid"
child_status=$?
set -e
echo "error: a local alpha process exited unexpectedly" >&2
((child_status != 0)) || child_status=1
exit "$child_status"
