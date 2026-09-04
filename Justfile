set shell := ["bash", "-euo", "pipefail", "-c"]

# List the available development commands.
default:
    @just --list

# Clear disposable state, install browser trust, and start a fresh example.
quickstart:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _quickstart; else exec nix develop -c just _quickstart; fi

[private]
_quickstart: _reset
    ./scripts/dev.sh --trust-browser

# Clear the disposable database, candidates, gateway data, and leaf certificate.
reset:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _reset; else exec nix develop -c just _reset; fi

[private]
_reset:
    ./scripts/reset-dev.sh

# Start the browser example without clearing its publication state.
start:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _start; else exec nix develop -c just _start; fi

[private]
_start:
    ./scripts/dev.sh --trust-browser

# Start without changing browser trust. Use this for CLI-only work.
start-cli:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _start-cli; else exec nix develop -c just _start-cli; fi

[private]
_start-cli:
    ./scripts/dev.sh

# Remove the development CA from supported browser stores.
untrust-browser:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _untrust-browser; else exec nix develop -c just _untrust-browser; fi

[private]
_untrust-browser:
    ./scripts/dev-gateway.sh --untrust-browser
