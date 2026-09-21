#!/usr/bin/env bash
set -euo pipefail

installer="$(cd -- "$(dirname -- "$0")/.." && pwd -P)/scripts/install-linux.sh"

awk '
  /^cd "\$repo_dir"$/ { installing = 1 }
  installing && /"\$cargo_bin" build --release --locked/ { build_line = NR }
  installing && /systemctl --user stop guardwsl.service/ && !stop_line { stop_line = NR }
  END {
    if (!build_line || !stop_line || build_line >= stop_line) {
      print "release build must finish before the active monitor is stopped" > "/dev/stderr"
      exit 1
    }
  }
' "$installer"
