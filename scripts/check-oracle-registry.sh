#!/usr/bin/env bash
# Keep the existing entry point for CI and local callers.
set -euo pipefail
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$script_dir/check-oracle-registry.py" "$@"
