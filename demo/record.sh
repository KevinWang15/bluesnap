#!/bin/sh
# Builds the app and records the README's demo GIFs (see record.py).
# Usage: demo/record.sh [scene ...]
set -e
cd "$(dirname "$0")/.."
cargo build -p bluesnap
exec python3 demo/record.py "$@"
