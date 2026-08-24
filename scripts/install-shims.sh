#!/usr/bin/env bash
set -euo pipefail

guard_bin="$HOME/.local/bin/guard"
if [[ $# -gt 0 ]]; then
  guard_bin="$1"
fi
shim_dir="$HOME/.local/lib/guardwsl/shims"
dispatcher="$shim_dir/.guardwsl-dispatcher-v1"
environment_dir="$HOME/.config/environment.d"
environment_file="$environment_dir/20-guardwsl.conf"

[[ -f "$guard_bin" && -x "$guard_bin" ]] || {
  printf 'Guard executable is missing or not executable: %s\n' "$guard_bin" >&2
  exit 1
}

install -d -m 0700 "$shim_dir"
temporary="$(mktemp "$shim_dir/.dispatcher.XXXXXX")"
trap 'rm -f -- "$temporary"' EXIT
cat >"$temporary" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
tool="$(basename -- "$0")"
case "$tool" in
  cargo|go|npm|npx|pnpm|yarn|bun|corepack|next|vite|tsc|docker|docker-compose|make|ninja|cmake|gradle|mvn|dotnet) ;;
  *)
    printf 'Unknown GuardWSL shim: %s\n' "$tool" >&2
    exit 126
    ;;
esac
exec "$HOME/.local/bin/guard" shim "$tool" "$@"
EOF
chmod 0700 "$temporary"
mv -f -- "$temporary" "$dispatcher"
trap - EXIT

# rustc is an internal Cargo detail. Intercepting it would make cargo test/check
# depend on the heavy-build preflight, which violates the GuardWSL contract.
retired_rustc="$shim_dir/rustc"
if [[ -L "$retired_rustc" ]] &&
  [[ "$(readlink -- "$retired_rustc")" == "$(basename -- "$dispatcher")" ]]; then
  rm -f -- "$retired_rustc"
fi

for tool in cargo go npm npx pnpm yarn bun corepack next vite tsc docker docker-compose make ninja cmake gradle mvn dotnet; do
  ln -sfn -- "$(basename -- "$dispatcher")" "$shim_dir/$tool"
done

path_block() {
  local startup_file="$1"
  [[ -L "$startup_file" ]] && {
    printf 'A shell startup file cannot be a symlink: %s\n' "$startup_file" >&2
    return 1
  }
  [[ -e "$startup_file" ]] || install -m 0600 /dev/null "$startup_file"
  if grep -Fq '# >>> GuardWSL shims >>>' "$startup_file"; then
    return 0
  fi
  cat >>"$startup_file" <<'EOF'

# >>> GuardWSL shims >>>
_guardwsl_shim_dir="$HOME/.local/lib/guardwsl/shims"
case "${PATH:-}" in
  "$_guardwsl_shim_dir"|"$_guardwsl_shim_dir":*) ;;
  *) export PATH="$_guardwsl_shim_dir${PATH:+:$PATH}" ;;
esac
unset _guardwsl_shim_dir
# <<< GuardWSL shims <<<
EOF
}

path_block "$HOME/.profile"
for startup_file in "$HOME/.bashrc" "$HOME/.zshrc"; do
  if [[ -e "$startup_file" ]]; then
    path_block "$startup_file"
  fi
done

[[ ! -L "$environment_file" ]] || {
  printf 'The environment file cannot be a symlink: %s\n' "$environment_file" >&2
  exit 1
}
install -d -m 0700 "$environment_dir"
environment_tmp="$(mktemp "$environment_dir/.guardwsl.XXXXXX")"
printf '# GuardWSL owned file\nPATH="%s:${PATH}"\n' "$shim_dir" >"$environment_tmp"
chmod 0600 "$environment_tmp"
mv -f -- "$environment_tmp" "$environment_file"

printf 'GuardWSL shims installed in %s\n' "$shim_dir"
