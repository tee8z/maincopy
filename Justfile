set shell := ["bash", "-euo", "pipefail", "-c"]

# List the available development commands.
default:
    @just --list

# Clear disposable state, install browser trust, and start a fresh example.
quickstart: reset
    ./scripts/dev.sh --trust-browser

# Clear the disposable database, candidates, gateway data, and leaf certificate.
reset:
    ./scripts/reset-dev.sh

# Start the browser example without clearing its publication state.
start:
    ./scripts/dev.sh --trust-browser

# Start without changing browser trust. Use this for CLI-only work.
start-cli:
    ./scripts/dev.sh

# Remove the development CA from supported browser stores.
untrust-browser:
    ./scripts/dev-gateway.sh --untrust-browser
