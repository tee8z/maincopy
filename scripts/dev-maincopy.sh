#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

die() {
  echo "error: $*" >&2
  exit 1
}

script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
readonly script_dir
project_root=$(cd -- "$script_dir/.." && pwd -P)
readonly project_root
readonly client="$project_root/target/debug/maincopy"
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

[[ -x $client ]] || die "the development client is not built; start scripts/dev-alpha.sh first"
[[ -f $root_certificate ]] || die "the development CA is missing; start scripts/dev-alpha.sh first"

exec "$client" \
  --admin-origin https://admin.localhost:8443 \
  --admin-ca-file "$root_certificate" \
  "$@"
