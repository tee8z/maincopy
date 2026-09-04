#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 077

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

for command in bash cp find flock grep ln mkdir mktemp rm; do
  command -v "$command" >/dev/null || die "$command is required"
done

case $# in
  0)
    script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
    reset_script="$script_dir/reset-dev.sh"
    ;;
  1) reset_script=$1 ;;
  *) die "usage: scripts/test-reset-dev.sh [RESET_SCRIPT]" ;;
esac
readonly reset_script
[[ -x $reset_script ]] || die "reset script is not executable: $reset_script"

test_root=$(mktemp -d "${TMPDIR:-/tmp}/maincopy-reset-test.XXXXXXXX")
readonly test_root
readonly project_root="$test_root/project"
readonly development_root="$project_root/target/maincopy-dev"
readonly data_root="$test_root/user-data"
readonly ca_root="$data_root/maincopy/dev-ca"
readonly fixture_script="$project_root/scripts/reset-dev.sh"
readonly output="$test_root/reset.log"

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

mkdir -p "$project_root/scripts" "$ca_root"
cp -- "$reset_script" "$fixture_script"
printf 'durable CA\n' >"$ca_root/rootCA.pem"
mkdir -p "$project_root/target/sibling"
printf 'keep\n' >"$project_root/target/sibling/marker"

run_reset() {
  XDG_DATA_HOME="$data_root" bash "$fixture_script" >"$output" 2>&1
}

prepare_state() {
  mkdir -p \
    "$development_root/state" \
    "$development_root/run" \
    "$development_root/gateway/data" \
    "$development_root/tls"
  printf 'candidate\n' >"$development_root/state/candidate"
  printf 'gateway\n' >"$development_root/gateway/data/state"
  printf 'certificate\n' >"$development_root/tls/maincopy-localhost.pem"
}

expect_lock_rejection() {
  local lock_path=$1
  local expected_message=$2
  local holder_pid
  local holder_read_fd
  local holder_write_fd
  local marker

  mkdir -p "${lock_path%/*}"
  : >"$lock_path"
  coproc LOCK_HOLDER {
    flock --exclusive "$lock_path" bash -c 'printf "locked\n"; read -r _'
  }
  holder_pid=$LOCK_HOLDER_PID
  holder_read_fd=${LOCK_HOLDER[0]}
  holder_write_fd=${LOCK_HOLDER[1]}
  read -r -u "$holder_read_fd" marker
  [[ $marker == locked ]] || die "the test lock owner did not become ready"

  if run_reset; then
    die "reset succeeded while $lock_path was locked"
  fi
  grep -F "$expected_message" "$output" >/dev/null ||
    die "reset returned the wrong lock diagnostic for $lock_path"
  [[ -d $development_root ]] || die "reset removed state while $lock_path was locked"

  printf '\n' >&"$holder_write_fd"
  wait "$holder_pid"
  exec {holder_read_fd}<&-
  exec {holder_write_fd}>&-
}

prepare_state
run_reset
[[ ! -e $development_root ]] || die "reset did not remove disposable development state"
grep -F "Removed disposable development state at $development_root" "$output" >/dev/null ||
  die "reset did not report the removed development state"
[[ -f $ca_root/rootCA.pem ]] || die "reset removed the durable development CA"
[[ -f $project_root/target/sibling/marker ]] || die "reset removed sibling target state"

run_reset
grep -F "No disposable development state exists at $development_root" "$output" >/dev/null ||
  die "reset was not idempotent when development state was absent"

outside_root="$test_root/outside-development-root"
mkdir -p "$project_root/target" "$outside_root"
printf 'keep\n' >"$outside_root/marker"
ln -s "$outside_root" "$development_root"
if run_reset; then
  die "reset accepted a symlinked development root"
fi
[[ -f $outside_root/marker ]] || die "reset followed a symlinked development root"
rm -- "$development_root"

rm -rf -- "$project_root/target"
outside_target="$test_root/outside-target"
mkdir -p "$outside_target/maincopy-dev"
printf 'keep\n' >"$outside_target/maincopy-dev/marker"
ln -s "$outside_target" "$project_root/target"
if run_reset; then
  die "reset accepted a symlinked target directory"
fi
[[ -f $outside_target/maincopy-dev/marker ]] || die "reset followed a symlinked target directory"
rm -- "$project_root/target"

prepare_state
expect_lock_rejection \
  "$development_root/run/maincopy.lock" \
  "maincopyd is running; stop it before resetting development state"
run_reset

prepare_state
expect_lock_rejection \
  "$development_root/state/maincopy.db.lock" \
  "maincopyd is running; stop it before resetting development state"
run_reset

prepare_state
expect_lock_rejection \
  "$ca_root/gateway.lock" \
  "the development gateway is running; stop it before resetting development state"
run_reset

[[ -f $ca_root/rootCA.pem ]] || die "the lock tests removed the durable development CA"
printf 'Development reset checks passed.\n'
