#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 077

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

case $# in
  2)
    operation=$1
    root_certificate=$2
    ;;
  *) die "usage: scripts/dev-browser-trust.sh install|uninstall ROOT_CA" ;;
esac
readonly operation root_certificate
case $operation in
  install | uninstall) ;;
  *) die "the browser trust operation must be install or uninstall" ;;
esac

for command in certutil openssl; do
  command -v "$command" >/dev/null || die "$command is required; run this inside nix develop"
done

[[ $root_certificate == /* ]] || die "the root CA path must be absolute"
[[ -f $root_certificate && ! -L $root_certificate ]] ||
  die "the root CA must be a regular file: $root_certificate"
openssl verify -CAfile "$root_certificate" "$root_certificate" >/dev/null 2>&1 ||
  die "the root CA is not a valid self-trusted certificate: $root_certificate"
root_fingerprint=$(
  openssl x509 -in "$root_certificate" -noout -fingerprint -sha256
) || die "the root CA fingerprint is unavailable: $root_certificate"
readonly root_fingerprint

if [[ -n ${XDG_CONFIG_HOME:-} ]]; then
  user_config_root=$XDG_CONFIG_HOME
elif [[ -n ${HOME:-} ]]; then
  user_config_root="$HOME/.config"
else
  die "neither XDG_CONFIG_HOME nor HOME identifies user configuration"
fi
[[ $user_config_root == /* ]] || die "the user configuration root must be an absolute path"
readonly user_config_root
readonly firefox_profiles_root="$user_config_root/mozilla/firefox"
readonly certificate_nickname="Maincopy development CA"

declare -a profiles=()
if [[ -d $firefox_profiles_root && ! -L $firefox_profiles_root ]]; then
  shopt -s nullglob
  for profile in "$firefox_profiles_root"/*; do
    [[ -d $profile && ! -L $profile ]] || continue
    [[ -f $profile/cert9.db && ! -L $profile/cert9.db ]] || continue
    profiles+=("$profile")
  done
  shopt -u nullglob
fi

declare -a installed=()
for profile in "${profiles[@]}"; do
  certutil -L -d "sql:$profile" >/dev/null 2>&1 ||
    die "the Firefox certificate database is not readable: $profile"

  certificate_pem=
  if certificate_pem=$(certutil \
    -L \
    -a \
    -d "sql:$profile" \
    -n "$certificate_nickname" 2>/dev/null); then
    installed_fingerprint=$(
      openssl x509 -noout -fingerprint -sha256 <<<"$certificate_pem"
    ) || die "the existing Maincopy certificate is invalid in Firefox profile: $profile"
    if [[ $installed_fingerprint != "$root_fingerprint" ]]; then
      die "a different certificate uses the Maincopy nickname in Firefox profile: $profile"
    fi
    installed+=(true)
  else
    installed+=(false)
  fi
done

for index in "${!profiles[@]}"; do
  profile=${profiles[index]}
  case $operation:${installed[index]} in
    install:true)
      certutil \
        -M \
        -d "sql:$profile" \
        -f /dev/null \
        -n "$certificate_nickname" \
        -t 'C,,' >/dev/null
      ;;
    install:false)
      certutil \
        -A \
        -d "sql:$profile" \
        -f /dev/null \
        -i "$root_certificate" \
        -n "$certificate_nickname" \
        -t 'C,,' >/dev/null
      ;;
    uninstall:true)
      certutil \
        -D \
        -d "sql:$profile" \
        -f /dev/null \
        -n "$certificate_nickname" >/dev/null
      ;;
    uninstall:false) ;;
  esac
done

case $operation in
  install)
    printf 'Installed the Maincopy development CA in %d XDG Firefox profile(s).\n' \
      "${#profiles[@]}"
    if ((${#profiles[@]} > 0)); then
      printf 'Restart Firefox before opening the Maincopy development origins.\n'
    fi
    ;;
  uninstall)
    printf 'Removed the Maincopy development CA from %d XDG Firefox profile(s).\n' \
      "${#profiles[@]}"
    ;;
esac
