set shell := ["bash", "-euo", "pipefail", "-c"]

# List the available development commands.
default:
    @just --list

# Clear disposable state, install browser trust, and start a fresh example.
[no-exit-message]
quickstart:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _quickstart; else exec nix develop -c just _quickstart; fi

[no-exit-message]
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
[no-exit-message]
start:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _start; else exec nix develop -c just _start; fi

[no-exit-message]
[private]
_start:
    ./scripts/dev.sh --trust-browser

# Start without changing browser trust. Use this for CLI-only work.
[no-exit-message]
start-cli:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _start-cli; else exec nix develop -c just _start-cli; fi

[no-exit-message]
[private]
_start-cli:
    ./scripts/dev.sh

# Remove the development CA from supported browser stores.
untrust-browser:
    @if [[ ${MAINCOPY_DEV_SHELL:-} == 1 ]]; then exec just _untrust-browser; else exec nix develop -c just _untrust-browser; fi

[private]
_untrust-browser:
    ./scripts/dev-gateway.sh --untrust-browser
