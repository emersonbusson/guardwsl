#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd -- "$(dirname -- "$0")/.." && pwd -P)"
temporary_home="$(mktemp -d)"
trap 'rm -rf -- "$temporary_home"' EXIT

home="$temporary_home/valid"
shim_dir="$home/.local/lib/guardwsl/shims"
environment_file="$home/.config/environment.d/20-guardwsl.conf"
mkdir -p "$shim_dir" "$home/.config/environment.d"
printf '#!/usr/bin/env bash\n' >"$shim_dir/.guardwsl-dispatcher-v1"
printf '%s\n' \
  '# GuardWSL managed shim artifacts v1' \
  'dispatcher=.guardwsl-dispatcher-v1' \
  'tools=cargo go npm npx pnpm yarn corepack docker docker-compose' \
  >"$shim_dir/.guardwsl-managed-v1"
ln -s .guardwsl-dispatcher-v1 "$shim_dir/go"
ln -s .guardwsl-dispatcher-v1 "$shim_dir/npm"
ln -s .guardwsl-dispatcher-v1 "$shim_dir/rustc"
printf 'preserve me\n' >"$shim_dir/custom-tool"
cat >"$home/.profile" <<'EOF'
before
# >>> GuardWSL shims >>>
PATH="/ignored"
# <<< GuardWSL shims <<<
after
EOF
cat >"$environment_file" <<EOF
# GuardWSL owned file
PATH="$shim_dir:\${PATH}"
LANG=C.UTF-8
EOF

HOME="$home" bash "$repo_dir/scripts/retire-shims.sh"
[[ ! -e "$shim_dir/go" && ! -L "$shim_dir/go" ]]
[[ ! -e "$shim_dir/npm" && ! -L "$shim_dir/npm" ]]
[[ ! -e "$shim_dir/rustc" && ! -L "$shim_dir/rustc" ]]
[[ ! -e "$shim_dir/.guardwsl-dispatcher-v1" ]]
[[ ! -e "$shim_dir/.guardwsl-managed-v1" ]]
[[ "$(cat "$shim_dir/custom-tool")" == 'preserve me' ]]
grep -Fqx before "$home/.profile"
grep -Fqx after "$home/.profile"
! grep -Fq 'GuardWSL shims' "$home/.profile"
grep -Fqx 'LANG=C.UTF-8' "$environment_file"
! grep -Fq "$shim_dir" "$environment_file"

home="$temporary_home/incomplete"
mkdir -p "$home"
printf 'before\n# >>> GuardWSL shims >>>\nPATH=partial\n' >"$home/.profile"
profile_before="$(cat "$home/.profile")"
if HOME="$home" bash "$repo_dir/scripts/retire-shims.sh" >/dev/null 2>&1; then
  printf 'incomplete PATH block must fail closed\n' >&2
  exit 1
fi
[[ "$(cat "$home/.profile")" == "$profile_before" ]]
