#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "$0")" && pwd -P)"
repo_dir="$(cd -- "$script_dir/.." && pwd -P)"
guard_bin="$HOME/.local/bin/guard"
unit_dir="$HOME/.config/systemd/user"
unit_path="$unit_dir/guardwsl.service"
config_path="$HOME/.config/guardwsl/config.toml"
config_lkg_path="$HOME/.config/guardwsl/config.last-good.toml"
shim_dir="$HOME/.local/lib/guardwsl/shims"
state_root="$HOME/.local/state/guardwsl"
distro_path="$state_root/distro-name"
environment_path="$HOME/.config/environment.d/20-guardwsl.conf"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
backup_root="$state_root/install-backups/$timestamp"
cargo_bin="$HOME/.cargo/bin/cargo"
distro_name="${WSL_DISTRO_NAME:-}"
real_tool_path=""
IFS=: read -r -a path_entries <<<"${PATH:-}"
for path_entry in "${path_entries[@]}"; do
  case "$path_entry" in
    *guardwsl/shims*) continue ;;
  esac
  real_tool_path="${real_tool_path:+$real_tool_path:}$path_entry"
done
[[ -n "$real_tool_path" ]] || {
  printf 'The real tool PATH is empty after removing GuardWSL shims.\n' >&2
  exit 1
}
runtime_dir="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
build_lock="$runtime_dir/guardwsl-build.lock"
maintenance_lock="$runtime_dir/guardwsl-maintenance.lock"
host_disk_floor_bytes=68719476736
host_memory_floor_bytes=12884901888
umask 077

[[ -n "$distro_name" ]] || {
  printf 'This installer must run inside WSL2.\n' >&2
  exit 1
}
[[ -x "$cargo_bin" ]] || {
  printf 'The real Cargo executable was not found at %s\n' "$cargo_bin" >&2
  exit 1
}
[[ "$distro_name" =~ ^[[:alnum:]._-]+$ ]] || {
  printf 'The WSL distribution name is unsafe for installation: %s\n' "$distro_name" >&2
  exit 1
}
[[ -d "$runtime_dir" && ! -L "$runtime_dir" ]] || {
  printf 'Invalid runtime directory: %s\n' "$runtime_dir" >&2
  exit 1
}
for lock_path in "$build_lock" "$maintenance_lock"; do
  [[ ! -L "$lock_path" ]] || {
    printf 'A lock cannot be a symlink: %s\n' "$lock_path" >&2
    exit 1
  }
done

sample_host() {
  local forwarded_wslenv="${WSLENV:-}"
  local -a host_sample=()
  if ! printf '%s\n' "$forwarded_wslenv" | tr ':' '\n' | cut -d/ -f1 | grep -Fxq GUARDWSL_DISTRO; then
    forwarded_wslenv="${forwarded_wslenv:+$forwarded_wslenv:}GUARDWSL_DISTRO"
  fi
  local powershell_bin="/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"
  [[ -x "$powershell_bin" ]] || powershell_bin="powershell.exe"
  mapfile -t host_sample < <(
    GUARDWSL_DISTRO="$distro_name" WSLENV="$forwarded_wslenv" \
      "$powershell_bin" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command '
$ErrorActionPreference = "Stop"
$distro = $env:GUARDWSL_DISTRO
$match = @(Get-ChildItem -LiteralPath "HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss" | ForEach-Object {
  $item = Get-ItemProperty -LiteralPath $_.PSPath
  if ($item.DistributionName -eq $distro) { $item }
})
if ($match.Count -ne 1) { throw "distribution was not found exactly once" }
$vhdx = Join-Path ([Environment]::ExpandEnvironmentVariables([string]$match[0].BasePath)) "ext4.vhdx"
try {
  $volume = Get-Volume -FilePath $vhdx -ErrorAction Stop
  $free = [uint64]$volume.SizeRemaining
} catch {
  $drive = [System.IO.DriveInfo]::new([System.IO.Path]::GetPathRoot($vhdx))
  $free = [uint64]$drive.AvailableFreeSpace
}
$os = Get-CimInstance Win32_OperatingSystem
[Console]::WriteLine($free)
[Console]::WriteLine([uint64]$os.FreePhysicalMemory * 1024)
' | tr -d '\000\r'
  )
  [[ "${host_sample[0]:-}" =~ ^[0-9]+$ && "${host_sample[1]:-}" =~ ^[0-9]+$ ]] || {
    printf 'Could not measure physical Windows disk and RAM.\n' >&2
    return 1
  }
  if (( host_sample[0] < host_disk_floor_bytes )); then
    printf 'Installation build blocked: the WSL backing volume has less than 64 GiB free.\n' >&2
    return 75
  fi
  if (( host_sample[1] < host_memory_floor_bytes )); then
    printf 'Installation build blocked: Windows has less than 12 GiB of available RAM.\n' >&2
    return 75
  fi
}

for managed_path in "$guard_bin" "$unit_path" "$config_path" "$config_lkg_path" "$shim_dir" "$distro_path" "$environment_path"; do
  if [[ -L "$managed_path" ]]; then
    printf 'A managed path cannot be a symlink: %s\n' "$managed_path" >&2
    exit 1
  fi
done

install -d -m 0700 "$backup_root" "$state_root" "$HOME/.local/bin" "$unit_dir"
old_active="$(systemctl --user is-active guardwsl.service 2>/dev/null || true)"
old_enabled="$(systemctl --user is-enabled guardwsl.service 2>/dev/null || true)"

if [[ -e "$guard_bin" ]]; then cp -a -- "$guard_bin" "$backup_root/guard"; fi
if [[ -e "$unit_path" ]]; then cp -a -- "$unit_path" "$backup_root/guardwsl.service"; fi
if [[ -e "$config_path" ]]; then cp -a -- "$config_path" "$backup_root/config.toml"; fi
if [[ -e "$config_lkg_path" ]]; then cp -a -- "$config_lkg_path" "$backup_root/config.last-good.toml"; fi
if [[ -d "$shim_dir" ]]; then cp -a -- "$shim_dir" "$backup_root/shims"; fi
if [[ -e "$distro_path" ]]; then cp -a -- "$distro_path" "$backup_root/distro-name"; fi
if [[ -e "$environment_path" ]]; then cp -a -- "$environment_path" "$backup_root/environment.conf"; fi
if [[ -e "$HOME/.profile" ]]; then cp -a -- "$HOME/.profile" "$backup_root/profile"; fi
if [[ -e "$HOME/.bashrc" ]]; then cp -a -- "$HOME/.bashrc" "$backup_root/bashrc"; fi
if [[ -e "$HOME/.zshrc" ]]; then cp -a -- "$HOME/.zshrc" "$backup_root/zshrc"; fi

rollback() {
  set +e
  systemctl --user stop guardwsl.service >/dev/null 2>&1
  if [[ -e "$backup_root/guard" ]]; then
    install -m 0755 "$backup_root/guard" "$guard_bin"
  else
    rm -f -- "$guard_bin"
  fi
  if [[ -e "$backup_root/guardwsl.service" ]]; then
    install -m 0644 "$backup_root/guardwsl.service" "$unit_path"
  else
    rm -f -- "$unit_path"
  fi
  if [[ -e "$backup_root/config.toml" ]]; then
    install -d -m 0700 "$(dirname -- "$config_path")"
    install -m 0600 "$backup_root/config.toml" "$config_path"
  else
    rm -f -- "$config_path"
  fi
  if [[ -e "$backup_root/config.last-good.toml" ]]; then
    install -m 0600 "$backup_root/config.last-good.toml" "$config_lkg_path"
  else
    rm -f -- "$config_lkg_path"
  fi
  if [[ -e "$backup_root/distro-name" ]]; then
    install -m 0600 "$backup_root/distro-name" "$distro_path"
  else
    rm -f -- "$distro_path"
  fi
  install -d -m 0700 "$(dirname -- "$environment_path")"
  if [[ -e "$backup_root/environment.conf" ]]; then
    cp -a -- "$backup_root/environment.conf" "$environment_path"
  else
    rm -f -- "$environment_path"
  fi
  for restore_spec in \
    "$backup_root/profile:$HOME/.profile" \
    "$backup_root/bashrc:$HOME/.bashrc" \
    "$backup_root/zshrc:$HOME/.zshrc"; do
    backup_file="${restore_spec%%:*}"
    startup_file="${restore_spec#*:}"
    if [[ -e "$backup_file" || -L "$backup_file" ]]; then
      rm -f -- "$startup_file"
      cp -a -- "$backup_file" "$startup_file"
    elif grep -Fq '# >>> GuardWSL shims >>>' "$startup_file" 2>/dev/null; then
      rm -f -- "$startup_file"
    fi
  done
  if [[ -d "$shim_dir" ]]; then
    mv -- "$shim_dir" "$backup_root/failed-shims"
  fi
  if [[ -d "$backup_root/shims" ]]; then
    cp -a -- "$backup_root/shims" "$shim_dir"
  fi
  systemctl --user daemon-reload >/dev/null 2>&1
  if [[ "$old_enabled" == "enabled" ]]; then
    systemctl --user enable guardwsl.service >/dev/null 2>&1
  else
    systemctl --user disable guardwsl.service >/dev/null 2>&1
  fi
  if [[ "$old_active" == "active" ]]; then
    systemctl --user start guardwsl.service >/dev/null 2>&1
  fi
  printf 'Installation rolled back. Backup: %s\n' "$backup_root" >&2
}
trap rollback ERR
trap 'rollback; exit 130' INT TERM

cd "$repo_dir"
PATH="$real_tool_path" "$cargo_bin" test --locked
exec {build_lock_fd}>"$build_lock"
chmod 0600 "$build_lock"
if ! flock -w 14400 "$build_lock_fd"; then
  printf 'Installation build blocked: another heavy build is still active.\n' >&2
  exit 75
fi
exec {maintenance_lock_fd}>"$maintenance_lock"
chmod 0600 "$maintenance_lock"
if ! flock -s -w 14400 "$maintenance_lock_fd"; then
  printf 'Installation build blocked: cleanup is still active.\n' >&2
  exit 75
fi
sample_host
if [[ "$old_active" == "active" ]]; then
  systemctl --user stop guardwsl.service
fi
PATH="$real_tool_path" "$cargo_bin" build --release --locked
flock -u "$maintenance_lock_fd"
exec {maintenance_lock_fd}>&-
flock -u "$build_lock_fd"
exec {build_lock_fd}>&-

install -m 0755 "$repo_dir/target/release/guard" "$guard_bin"
install -m 0644 "$repo_dir/systemd/guardwsl.service" "$unit_path"
chmod 0600 "$config_path" 2>/dev/null || true
if [[ -e "$config_path" ]]; then
  "$guard_bin" config normalize
else
  "$guard_bin" config init
fi
distro_tmp="$(mktemp "$state_root/.distro-name.XXXXXX")"
printf '%s\n' "$distro_name" >"$distro_tmp"
chmod 0600 "$distro_tmp"
mv -f -- "$distro_tmp" "$distro_path"
bash "$repo_dir/scripts/install-shims.sh" "$guard_bin"

systemctl --user daemon-reload
systemctl --user enable guardwsl.service
systemctl --user restart guardwsl.service

healthy=false
for _ in $(seq 1 30); do
  if "$guard_bin" doctor --json >/dev/null 2>&1; then
    healthy=true
    break
  fi
  sleep 1
done
[[ "$healthy" == "true" ]] || {
  printf 'GuardWSL did not become healthy within 30 seconds.\n' >&2
  false
}

trap - ERR INT TERM
printf '{"installed_at":"%s","backup":"%s","version":"%s"}\n' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  "$backup_root" \
  "$("$guard_bin" --version | awk '{print $2}')" \
  >"$state_root/install.json"
chmod 0600 "$state_root/install.json"

printf 'GuardWSL is installed and active. Backup: %s\n' "$backup_root"
