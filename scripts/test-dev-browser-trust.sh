#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 077

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

for command in bash certutil find grep ln mkdir mktemp mv openssl; do
  command -v "$command" >/dev/null || die "$command is required"
done

case $# in
  0)
    script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
    trust_script="$script_dir/dev-browser-trust.sh"
    ;;
  1) trust_script=$1 ;;
  *) die "usage: scripts/test-dev-browser-trust.sh [TRUST_SCRIPT]" ;;
esac
readonly trust_script
[[ -x $trust_script ]] || die "browser trust script is not executable: $trust_script"

test_root=$(mktemp -d "${TMPDIR:-/tmp}/maincopy-browser-trust-test.XXXXXXXX")
readonly test_root
readonly config_root="$test_root/config"
readonly firefox_root="$config_root/mozilla/firefox"
readonly root_certificate="$test_root/root-ca.pem"
readonly root_private_key="$test_root/root-ca-key.pem"
readonly other_certificate="$test_root/other-root-ca.pem"
readonly other_private_key="$test_root/other-root-ca-key.pem"
readonly primary_profile="$firefox_root/00-primary"
readonly empty_profile="$firefox_root/10-empty"
readonly collision_profile="$firefox_root/99-collision"
readonly outside_profile="$test_root/outside-profile"
readonly linked_database_profile="$firefox_root/20-linked-database"
readonly linked_database="$test_root/linked-cert9.db"
readonly nickname="Maincopy development CA"
readonly output="$test_root/trust.log"

cleanup() {
  local status=$?
  trap - EXIT HUP INT TERM
  find "$test_root" -depth -delete
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

create_root_ca() {
  local certificate=$1
  local private_key=$2
  local common_name=$3
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "$private_key" \
    -out "$certificate" \
    -days 1 \
    -sha256 \
    -subj "/CN=$common_name" \
    -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    >/dev/null 2>&1
}

create_profile() {
  local profile=$1
  mkdir -p "$profile"
  certutil -N --empty-password -d "sql:$profile"
}

certificate_fingerprint() {
  openssl x509 -in "$1" -noout -fingerprint -sha256
}

installed_fingerprint() {
  local profile=$1
  certutil -L -a -d "sql:$profile" -n "$nickname" |
    openssl x509 -noout -fingerprint -sha256
}

assert_not_installed() {
  local profile=$1
  if certutil -L -a -d "sql:$profile" -n "$nickname" >/dev/null 2>&1; then
    die "the Maincopy CA was unexpectedly installed in $profile"
  fi
}

run_trust() {
  XDG_CONFIG_HOME="$config_root" \
    HOME="$test_root/home" \
    bash "$trust_script" "$@" >"$output" 2>&1
}

mkdir -p "$firefox_root"
create_root_ca "$root_certificate" "$root_private_key" "Maincopy browser trust test root"
create_root_ca "$other_certificate" "$other_private_key" "Unrelated browser trust test root"
create_profile "$primary_profile"
create_profile "$outside_profile"
ln -s "$outside_profile" "$firefox_root/linked-profile"

create_profile "$linked_database_profile"
mv -- "$linked_database_profile/cert9.db" "$linked_database"
ln -s "$linked_database" "$linked_database_profile/cert9.db"

run_trust install "$root_certificate"
grep -F 'Installed the Maincopy development CA in 1 XDG Firefox profile(s).' \
  "$output" >/dev/null || die "install reported the wrong Firefox profile count"
grep -F 'Restart Firefox before opening the Maincopy development origins.' \
  "$output" >/dev/null || die "install omitted the Firefox restart instruction"
[[ $(installed_fingerprint "$primary_profile") == \
  "$(certificate_fingerprint "$root_certificate")" ]] ||
  die "install stored the wrong root CA"
certutil -L -d "sql:$primary_profile" |
  grep -E '^Maincopy development CA[[:space:]]+C,,' >/dev/null ||
  die "install did not grant the Maincopy CA server trust"
assert_not_installed "$outside_profile"
assert_not_installed "$linked_database_profile"

run_trust install "$root_certificate"
[[ $(certutil -L -d "sql:$primary_profile" | grep -Fc "$nickname") == 1 ]] ||
  die "repeat install duplicated the Maincopy CA"

create_profile "$empty_profile"
create_profile "$collision_profile"
certutil \
  -A \
  -d "sql:$collision_profile" \
  -f /dev/null \
  -i "$other_certificate" \
  -n "$nickname" \
  -t 'C,,' >/dev/null

if run_trust install "$root_certificate"; then
  die "install overwrote a different certificate under the Maincopy nickname"
fi
grep -F 'a different certificate uses the Maincopy nickname' "$output" >/dev/null ||
  die "nickname collision returned the wrong install diagnostic"
assert_not_installed "$empty_profile"
[[ $(installed_fingerprint "$primary_profile") == \
  "$(certificate_fingerprint "$root_certificate")" ]] ||
  die "collision handling changed a valid earlier profile"
[[ $(installed_fingerprint "$collision_profile") == \
  "$(certificate_fingerprint "$other_certificate")" ]] ||
  die "collision handling overwrote the conflicting certificate"

if run_trust uninstall "$root_certificate"; then
  die "uninstall deleted a different certificate under the Maincopy nickname"
fi
grep -F 'a different certificate uses the Maincopy nickname' "$output" >/dev/null ||
  die "nickname collision returned the wrong uninstall diagnostic"
[[ $(installed_fingerprint "$primary_profile") == \
  "$(certificate_fingerprint "$root_certificate")" ]] ||
  die "collision handling partially uninstalled an earlier profile"

certutil \
  -D \
  -d "sql:$collision_profile" \
  -f /dev/null \
  -n "$nickname" >/dev/null
run_trust uninstall "$root_certificate"
assert_not_installed "$primary_profile"
assert_not_installed "$empty_profile"
assert_not_installed "$collision_profile"
run_trust uninstall "$root_certificate"

printf 'Development browser trust checks passed.\n'
