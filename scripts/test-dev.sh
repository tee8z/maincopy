#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 077

fixture_event() {
  printf '%s\n' "$1" >>"$MAINCOPY_DEV_TEST_ROOT/events"
}

run_fixture_command() {
  case $MAINCOPY_DEV_TEST_COMMAND in
    cargo)
      if [[ $MAINCOPY_DEV_TEST_MODE == build-failure ]]; then
        printf 'fixture build failed\n' >&2
        exit 42
      fi
      fixture_event cargo
      ;;
    curl)
      local url=
      local argument
      for argument in "$@"; do
        case $argument in
          http://* | https://*) url=$argument ;;
        esac
      done
      case "$MAINCOPY_DEV_TEST_MODE:$url" in
        success:http://127.0.0.1:3000/health/ready)
          [[ -f $MAINCOPY_DEV_TEST_ROOT/daemon-started ]] || exit 7
          fixture_event daemon-ready
          : >"$MAINCOPY_DEV_TEST_ROOT/daemon-ready"
          ;;
        timeout:http://127.0.0.1:3000/health/ready)
          exit 22
          ;;
        success:https://maincopy.localhost:8443/health/ready)
          [[ -f $MAINCOPY_DEV_TEST_ROOT/gateway-started ]] || exit 7
          fixture_event public-ready
          ;;
        success:https://admin.localhost:8443/admin/login)
          [[ -f $MAINCOPY_DEV_TEST_ROOT/gateway-started ]] || exit 7
          fixture_event admin-ready
          ;;
        *)
          fixture_event unexpected-curl
          exit 64
          ;;
      esac
      ;;
    maincopyd)
      fixture_event daemon-started
      printf '%s\n' "$$" >"$MAINCOPY_DEV_TEST_ROOT/daemon.pid"
      : >"$MAINCOPY_DEV_TEST_ROOT/daemon-started"
      exec tail -f /dev/null
      ;;
    reset-dev.sh)
      fixture_event reset
      ;;
    nix)
      [[ $# == 4 && $1 == develop && $2 == -c && $3 == just ]] || exit 64
      fixture_event nix
      shift 2
      export MAINCOPY_DEV_SHELL=1
      exec "$@"
      ;;
    dev-gateway.sh)
      if [[ ! -f $MAINCOPY_DEV_TEST_ROOT/daemon-ready ]]; then
        fixture_event gateway-before-daemon-ready
        exit 70
      fi
      fixture_event gateway-started
      printf '%s\n' "$$" >"$MAINCOPY_DEV_TEST_ROOT/gateway.pid"
      : >"$MAINCOPY_DEV_TEST_ROOT/gateway-started"
      mkdir -p "$XDG_DATA_HOME/maincopy/dev-ca"
      : >"$XDG_DATA_HOME/maincopy/dev-ca/rootCA.pem"
      exec tail -f /dev/null
      ;;
    *)
      printf 'error: unknown development launcher fixture command: %s\n' \
        "$MAINCOPY_DEV_TEST_COMMAND" >&2
      exit 64
      ;;
  esac
}

if [[ -n ${MAINCOPY_DEV_TEST_COMMAND:-} ]]; then
  run_fixture_command "$@"
  exit
fi

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

for command in bash chmod cp env find grep just kill mkdir mktemp mv sed setsid tail timeout; do
  command -v "$command" >/dev/null || die "$command is required"
done

case $# in
  0)
    script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
    production_launcher="$script_dir/dev.sh"
    production_justfile="$script_dir/../Justfile"
    ;;
  2) production_launcher=$1; production_justfile=$2 ;;
  *) die "usage: scripts/test-dev.sh [DEV_SCRIPT JUSTFILE]" ;;
esac
readonly production_launcher production_justfile
[[ -x $production_launcher ]] || die "development launcher is not executable: $production_launcher"

script_path=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)/${BASH_SOURCE[0]##*/}
readonly script_path
fixture_bash=$(command -v bash)
readonly fixture_bash
[[ $fixture_bash == /* ]] || die "the fixture Bash path is not absolute: $fixture_bash"
test_root=$(mktemp -d "${TMPDIR:-/tmp}/maincopy-dev-launcher-test.XXXXXXXX")
readonly test_root
launcher_pid=

cleanup() {
  local status=$?
  trap - EXIT HUP INT TERM
  if [[ -n $launcher_pid ]] && kill -0 "$launcher_pid" 2>/dev/null; then
    kill -TERM "$launcher_pid" 2>/dev/null || true
    wait "$launcher_pid" 2>/dev/null || true
  fi
  find "$test_root" -depth -delete
  exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

create_fixture_command() {
  local path=$1
  local command=$2
  printf '#!%s\n' "$fixture_bash" >"$path"
  printf \
    'MAINCOPY_DEV_TEST_COMMAND=%q exec %q %q "$@"\n' \
    "$command" \
    "$fixture_bash" \
    "$script_path" >>"$path"
  chmod 700 "$path"
  [[ -x $path ]] || die "fixture command is not executable: $path"
}

prepare_scenario() {
  local scenario_root=$1
  local project_root="$scenario_root/project"
  mkdir -p \
    "$project_root/scripts" \
    "$project_root/target/debug" \
    "$scenario_root/bin" \
    "$scenario_root/user-data"
  # The Nix sandbox has no /usr/bin/env for just's direct script invocation.
  sed "1s|.*|#!$fixture_bash|" "$production_launcher" >"$project_root/scripts/dev.sh"
  chmod 700 "$project_root/scripts/dev.sh"
  cp -- "$production_justfile" "$project_root/Justfile"
  create_fixture_command "$project_root/scripts/reset-dev.sh" reset-dev.sh
  create_fixture_command "$project_root/scripts/dev-gateway.sh" dev-gateway.sh
  create_fixture_command "$project_root/target/debug/maincopyd" maincopyd
  create_fixture_command "$scenario_root/bin/cargo" cargo
  create_fixture_command "$scenario_root/bin/curl" curl
  create_fixture_command "$scenario_root/bin/caddy" caddy
  create_fixture_command "$scenario_root/bin/flock" flock
  create_fixture_command "$scenario_root/bin/mkcert" mkcert
  create_fixture_command "$scenario_root/bin/nix" nix
  : >"$scenario_root/events"
}

assert_events() {
  local scenario_root=$1
  shift
  local -a actual
  mapfile -t actual <"$scenario_root/events"
  if ((${#actual[@]} != $#)); then
    die "expected $# launcher events but observed ${#actual[@]}: ${actual[*]}"
  fi
  local index=0
  local expected
  for expected in "$@"; do
    [[ ${actual[index]} == "$expected" ]] ||
      die "launcher event $((index + 1)) was ${actual[index]}, expected $expected"
    index=$((index + 1))
  done
}

assert_process_stopped() {
  local pid_file=$1
  local name=$2
  local pid
  [[ -f $pid_file ]] || die "$name did not record its process ID"
  read -r pid <"$pid_file"
  if kill -0 "$pid" 2>/dev/null; then
    die "$name remained alive after launcher shutdown"
  fi
}

for signal in INT TERM; do
  success_root="$test_root/success-$signal"
  prepare_scenario "$success_root"
  env --default-signal=INT \
    MAINCOPY_DEV_TEST_MODE=success \
    MAINCOPY_DEV_TEST_ROOT="$success_root" \
    XDG_DATA_HOME="$success_root/user-data" \
    PATH="$success_root/bin:$PATH" \
    bash "$success_root/project/scripts/dev.sh" \
    >"$success_root/launcher.log" 2>&1 </dev/null &
  launcher_pid=$!

  # Follow an observable condition instead of guessing how long startup needs.
  # The single-quoted program receives the launcher PID and log as positional arguments.
  # shellcheck disable=SC2016
  if ! timeout 5s sh -c '
    tail --pid="$1" -n +1 -f "$2" |
      grep -Fqm1 "Maincopy development environment is ready."
  ' sh "$launcher_pid" "$success_root/launcher.log"; then
    die "the fixture launcher did not report readiness: $(<"$success_root/launcher.log")"
  fi

  kill -"$signal" "$launcher_pid"
  launcher_status=0
  wait "$launcher_pid" || launcher_status=$?
  launcher_pid=
  expected_status=130
  [[ $signal != TERM ]] || expected_status=143
  ((launcher_status == expected_status)) ||
    die "the fixture launcher returned $launcher_status after SIG$signal, expected $expected_status"
  assert_events \
    "$success_root" \
    cargo \
    daemon-started \
    daemon-ready \
    gateway-started \
    public-ready \
    admin-ready
  assert_process_stopped "$success_root/daemon.pid" maincopyd
  assert_process_stopped "$success_root/gateway.pid" gateway
done

for shell_mode in 0 1; do
  for recipe in quickstart start start-cli; do
    recipe_root="$test_root/just-$shell_mode-$recipe"
    prepare_scenario "$recipe_root"
    env --default-signal=INT \
      MAINCOPY_DEV_SHELL="$shell_mode" \
      MAINCOPY_DEV_TEST_MODE=success \
      MAINCOPY_DEV_TEST_ROOT="$recipe_root" \
      XDG_DATA_HOME="$recipe_root/user-data" \
      PATH="$recipe_root/bin:$PATH" \
      setsid just --justfile "$recipe_root/project/Justfile" "$recipe" \
      >"$recipe_root/launcher.log" 2>&1 </dev/null &
    launcher_pid=$!

    # shellcheck disable=SC2016
    if ! timeout 5s sh -c '
      tail --pid="$1" -n +1 -f "$2" |
        grep -Fqm1 "Maincopy development environment is ready."
    ' sh "$launcher_pid" "$recipe_root/launcher.log"; then
      die "just $recipe did not report readiness: $(<"$recipe_root/launcher.log")"
    fi

    # Terminal Ctrl+C reaches every foreground process, including both just invocations.
    kill -INT -- "-$launcher_pid"
    launcher_status=0
    wait "$launcher_pid" || launcher_status=$?
    launcher_pid=
    ((launcher_status == 130)) || die "just $recipe lost the interrupt status: $launcher_status"
    if grep -F 'error:' "$recipe_root/launcher.log"; then
      die "just $recipe reported orderly interruption as an error"
    fi
    assert_process_stopped "$recipe_root/daemon.pid" maincopyd
    assert_process_stopped "$recipe_root/gateway.pid" gateway
    if [[ $shell_mode == 0 ]]; then
      grep -Fxq nix "$recipe_root/events" || die "just $recipe did not enter the Nix shell"
    fi

    failure_status=0
    env MAINCOPY_DEV_SHELL="$shell_mode" MAINCOPY_DEV_TEST_MODE=build-failure \
      MAINCOPY_DEV_TEST_ROOT="$recipe_root" \
      XDG_DATA_HOME="$recipe_root/user-data" PATH="$recipe_root/bin:$PATH" \
      just --justfile "$recipe_root/project/Justfile" "$recipe" \
      >"$recipe_root/failure.log" 2>&1 || failure_status=$?
    ((failure_status == 42)) || die "just $recipe hid a build failure: $failure_status"
    grep -Fq 'fixture build failed' "$recipe_root/failure.log" ||
      die "just $recipe hid the build diagnostic"
  done
done

readonly timeout_root="$test_root/timeout"
prepare_scenario "$timeout_root"
readonly timeout_launcher="$timeout_root/project/scripts/dev.sh"
timeout_constant_count=$(grep -c '^readonly readiness_timeout_seconds=30$' "$timeout_launcher")
((timeout_constant_count == 1)) ||
  die "the launcher readiness timeout constant changed unexpectedly"
sed \
  's/^readonly readiness_timeout_seconds=30$/readonly readiness_timeout_seconds=1/' \
  "$timeout_launcher" >"$timeout_launcher.next"
mv -- "$timeout_launcher.next" "$timeout_launcher"

timeout_status=0
env \
  MAINCOPY_DEV_TEST_MODE=timeout \
  MAINCOPY_DEV_TEST_ROOT="$timeout_root" \
  XDG_DATA_HOME="$timeout_root/user-data" \
  PATH="$timeout_root/bin:$PATH" \
  timeout 5s bash "$timeout_launcher" \
  >"$timeout_root/launcher.log" 2>&1 </dev/null || timeout_status=$?
((timeout_status == 1)) ||
  die "the unready fixture launcher returned $timeout_status, expected its own status 1"
grep -F \
  'error: maincopyd did not become ready within 1 seconds' \
  "$timeout_root/launcher.log" >/dev/null ||
  die "the unready fixture launcher returned the wrong timeout diagnostic"
assert_events "$timeout_root" cargo daemon-started
[[ ! -e $timeout_root/gateway-started ]] ||
  die "the gateway started before maincopyd became ready"
[[ ! -e $timeout_root/gateway.pid ]] ||
  die "the timeout scenario launched the gateway process"
assert_process_stopped "$timeout_root/daemon.pid" maincopyd

printf 'Development launcher checks passed.\n'
