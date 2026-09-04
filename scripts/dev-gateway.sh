#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/dev-gateway.sh [--trust-browser | --untrust-browser]

Create an isolated development CA and serve the local Maincopy HTTPS gateway.
--trust-browser explicitly installs that CA into supported user NSS stores.
--untrust-browser removes that CA from those stores and exits.
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

trust_browser=false
untrust_browser=false
case ${1:-} in
  "") ;;
  --trust-browser) trust_browser=true ;;
  --untrust-browser) untrust_browser=true ;;
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

for command in caddy flock mkcert mkdir mktemp mv pwd rm rmdir; do
  command -v "$command" >/dev/null || die "$command is required; run this inside nix develop"
done

script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
readonly script_dir
project_root=$(cd -- "$script_dir/.." && pwd -P)
readonly project_root
readonly gateway_config="$project_root/dev/Caddyfile"
readonly browser_trust="$script_dir/dev-browser-trust.sh"
readonly tls_root="$project_root/target/maincopy-dev/tls"
readonly certificate="$tls_root/maincopy-localhost.pem"
readonly private_key="$tls_root/maincopy-localhost-key.pem"
if [[ -n ${XDG_DATA_HOME:-} ]]; then
  maincopy_data_root=$XDG_DATA_HOME
elif [[ -n ${HOME:-} ]]; then
  maincopy_data_root="$HOME/.local/share"
else
  die "neither XDG_DATA_HOME nor HOME identifies durable user state"
fi
[[ $maincopy_data_root == /* ]] || die "the development data root must be an absolute path"
readonly maincopy_data_root
readonly ca_root="$maincopy_data_root/maincopy/dev-ca"
readonly root_certificate="$ca_root/rootCA.pem"
readonly root_private_key="$ca_root/rootCA-key.pem"
readonly caddy_data="$project_root/target/maincopy-dev/gateway/data"
readonly caddy_config="$project_root/target/maincopy-dev/gateway/config"
[[ -x $browser_trust ]] || die "the browser trust helper is not executable: $browser_trust"

if $untrust_browser; then
  [[ -f $root_certificate ]] || die "the Maincopy development CA does not exist"
else
  mkdir -p "$ca_root" "$tls_root" "$caddy_data" "$caddy_config"
fi

exec {gateway_lock_fd}>>"$ca_root/gateway.lock"
readonly gateway_lock_fd
flock --exclusive --nonblock "$gateway_lock_fd" ||
  die "another Maincopy development gateway owns $ca_root/gateway.lock"

if $untrust_browser; then
  "$browser_trust" uninstall "$root_certificate"
  CAROOT="$ca_root" TRUST_STORES=nss mkcert -uninstall
  echo "Removed the Maincopy development CA from supported user browser stores."
  exit 0
fi

ca_files=0
for path in "$root_certificate" "$root_private_key"; do
  [[ ! -e $path ]] || ca_files=$((ca_files + 1))
done
((ca_files != 1)) || die "the durable Maincopy development CA is incomplete at $ca_root"

leaf_staging=$(mktemp -d "$tls_root/.leaf.XXXXXXXX")
readonly leaf_staging
readonly staged_certificate="$leaf_staging/certificate.pem"
readonly staged_private_key="$leaf_staging/private-key.pem"
# shellcheck disable=SC2329 # Invoked indirectly by the EXIT trap.
cleanup_leaf_staging() {
  rm -f -- "$staged_certificate" "$staged_private_key"
  rmdir -- "$leaf_staging" 2>/dev/null || true
}
trap cleanup_leaf_staging EXIT
CAROOT="$ca_root" mkcert \
  -cert-file "$staged_certificate" \
  -key-file "$staged_private_key" \
  admin.localhost maincopy.localhost localhost 127.0.0.1 ::1
mv -- "$staged_certificate" "$certificate"
mv -- "$staged_private_key" "$private_key"
rmdir -- "$leaf_staging"
trap - EXIT

if $trust_browser; then
  echo "Installing the isolated Maincopy development CA into supported user browser stores..."
  CAROOT="$ca_root" TRUST_STORES=nss mkcert -install
  "$browser_trust" install "$root_certificate"
fi

echo "Maincopy development CA: $root_certificate"
echo "Public origin: https://maincopy.localhost:8443"
echo "Admin origin:  https://admin.localhost:8443"

export MAINCOPY_DEV_TLS_CERTIFICATE="$certificate"
export MAINCOPY_DEV_TLS_PRIVATE_KEY="$private_key"
export XDG_DATA_HOME="$caddy_data"
export XDG_CONFIG_HOME="$caddy_config"
exec caddy run --config "$gateway_config" --adapter caddyfile
