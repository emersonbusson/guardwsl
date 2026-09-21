#!/usr/bin/env bash

guard_platform_from_release() {
  local release="${1,,}"
  if [[ "$release" == *microsoft* ]]; then
    if [[ "$release" == *wsl2* ]]; then
      printf 'wsl2\n'
    else
      printf 'WSL1 is not supported\n' >&2
      return 1
    fi
  else
    printf 'linux\n'
  fi
}
