#!/usr/bin/env bash
set -euo pipefail

# Compilation and all validation run inside Nix derivations.
exec nix flake check . -L "$@"
