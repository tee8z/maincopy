#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

for command in flock pwd rm; do
  command -v "$command" >/dev/null ||
    die "$command is required; run this inside nix develop"
done

script_dir=$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
readonly script_dir
project_root=$(cd -- "$script_dir/.." && pwd -P)
readonly project_root
readonly target_root="$project_root/target"
readonly development_root="$project_root/target/maincopy-dev"
readonly state_lock="$development_root/state/maincopy.db.lock"
readonly runtime_lock="$development_root/run/maincopy.lock"

if [[ -n ${XDG_DATA_HOME:-} ]]; then
  maincopy_data_root=$XDG_DATA_HOME
elif [[ -n ${HOME:-} ]]; then
  maincopy_data_root="$HOME/.local/share"
else
  die "neither XDG_DATA_HOME nor HOME identifies durable user state"
fi
[[ $maincopy_data_root == /* ]] || die "the development data root must be absolute"
readonly gateway_lock="$maincopy_data_root/maincopy/dev-ca/gateway.lock"

[[ ! -L $target_root ]] || die "refusing symlinked target directory $target_root"
if [[ -e $development_root || -L $development_root ]]; then
  [[ -d $development_root && ! -L $development_root ]] ||
    die "refusing non-directory or symlink $development_root"
  for private_root in "$development_root/state" "$development_root/run"; do
    if [[ -e $private_root || -L $private_root ]]; then
      [[ -d $private_root && ! -L $private_root ]] ||
        die "refusing non-directory or symlink $private_root"
    fi
  done
fi

for lock_path in "$state_lock" "$runtime_lock" "$gateway_lock"; do
  if [[ -e $lock_path || -L $lock_path ]]; then
    [[ -f $lock_path && ! -L $lock_path ]] || die "refusing unsafe lock path $lock_path"
  fi
done

if [[ -e $runtime_lock ]]; then
  exec {runtime_lock_fd}>>"$runtime_lock"
  flock --exclusive --nonblock "$runtime_lock_fd" ||
    die "maincopyd is running; stop it before resetting development state"
fi
if [[ -e $state_lock ]]; then
  exec {state_lock_fd}>>"$state_lock"
  flock --exclusive --nonblock "$state_lock_fd" ||
    die "maincopyd is running; stop it before resetting development state"
fi
if [[ -e $gateway_lock ]]; then
  exec {gateway_lock_fd}>>"$gateway_lock"
  flock --exclusive --nonblock "$gateway_lock_fd" ||
    die "the development gateway is running; stop it before resetting development state"
fi

if [[ ! -e $development_root && ! -L $development_root ]]; then
  printf 'No disposable development state exists at %s\n' "$development_root"
  exit 0
fi

[[ $development_root == "$project_root/target/maincopy-dev" ]] ||
  die "refusing unexpected development path $development_root"

rm -rf -- "$development_root"
printf 'Removed disposable development state at %s\n' "$development_root"
