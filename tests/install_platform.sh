#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "$0")" && pwd -P)"
source "$script_dir/../scripts/platform.sh"

[[ "$(guard_platform_from_release '6.12.0-42-generic')" == linux ]]
[[ "$(guard_platform_from_release '6.18.40.1-microsoft-standard-WSL2+')" == wsl2 ]]
if guard_platform_from_release '4.4.0-Microsoft' >/dev/null 2>&1; then
  printf 'WSL1 must not be accepted by the installer\n' >&2
  exit 1
fi
