#!/usr/bin/env bash
set -euo pipefail

shim_dir="$HOME/.local/lib/guardwsl/shims"
environment_file="$HOME/.config/environment.d/20-guardwsl.conf"
start_marker='# >>> GuardWSL shims >>>'
end_marker='# <<< GuardWSL shims <<<'
path_line="PATH=\"$shim_dir:\${PATH}\""

startup_files=("$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc")
for file in "${startup_files[@]}" "$environment_file"; do
  if [[ -L "$file" ]]; then
    printf 'Refusing to edit a symlink: %s\n' "$file" >&2
    exit 1
  fi
done
if [[ -L "$shim_dir" ]]; then
  printf 'Refusing to inspect a symlink: %s\n' "$shim_dir" >&2
  exit 1
fi

remove_marked_path_block() {
  local file="$1"
  [[ -f "$file" ]] || return 0
  if ! grep -Fqx "$start_marker" "$file"; then
    return 0
  fi
  local temporary
  temporary="$(mktemp "${file}.guardwsl.XXXXXX")"
  if ! awk -v start="$start_marker" -v end="$end_marker" '
    $0 == start {
      if (inside || ++starts > 1) exit 3
      inside = 1
      next
    }
    $0 == end {
      if (!inside || ++ends > 1) exit 3
      inside = 0
      next
    }
    !inside { print }
    END {
      if (inside || starts != 1 || ends != 1) exit 3
    }
  ' "$file" >"$temporary"; then
    rm -f -- "$temporary"
    printf 'Refusing to edit an incomplete or repeated GuardWSL block: %s\n' "$file" >&2
    return 1
  fi
  chmod --reference="$file" "$temporary"
  mv -f -- "$temporary" "$file"
}

for file in "${startup_files[@]}"; do
  remove_marked_path_block "$file"
done

if [[ -f "$environment_file" ]] && grep -Fqx "$path_line" "$environment_file"; then
  temporary="$(mktemp "${environment_file}.guardwsl.XXXXXX")"
  awk -v path="$path_line" '$0 != path && $0 != "# GuardWSL owned file" { print }' \
    "$environment_file" >"$temporary"
  if [[ ! -s "$temporary" ]]; then
    rm -f -- "$temporary" "$environment_file"
  else
    chmod --reference="$environment_file" "$temporary"
    mv -f -- "$temporary" "$environment_file"
  fi
fi

if [[ -d "$shim_dir" ]]; then
  shopt -s nullglob dotglob
  for path in "$shim_dir"/*; do
    name="${path##*/}"
    case "$name" in
      .guardwsl-dispatcher-v1)
        if [[ -f "$path" && ! -L "$path" ]]; then
          rm -f -- "$path"
        fi
        ;;
      .guardwsl-managed-v1)
        if [[ -f "$path" && ! -L "$path" ]] &&
          printf '%s\n' \
            '# GuardWSL managed shim artifacts v1' \
            'dispatcher=.guardwsl-dispatcher-v1' \
            'tools=cargo go npm npx pnpm yarn corepack docker docker-compose' |
            cmp -s - "$path"; then
          rm -f -- "$path"
        fi
        ;;
      cargo|go|npm|npx|pnpm|yarn|bun|corepack|next|vite|tsc|docker|docker-compose|make|ninja|cmake|gradle|mvn|dotnet|rustc)
        if [[ -L "$path" && "$(readlink -- "$path")" == ".guardwsl-dispatcher-v1" ]]; then
          rm -f -- "$path"
        fi
        ;;
      *)
        printf 'Preserving unrecognized shim path: %s\n' "$path" >&2
        ;;
    esac
  done
  shopt -u nullglob dotglob
  rmdir -- "$shim_dir" 2>/dev/null || true
fi

printf 'Retired known GuardWSL tool shims and marked PATH entries.\n'
