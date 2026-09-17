#!/usr/bin/env bash
set -euo pipefail

die() { printf 'sn46-validator: %s\n' "$*" >&2; exit 1; }

prompt() {
    local answer
    printf '%s [%s]: ' "$1" "$2" > /dev/tty
    IFS= read -r answer < /dev/tty || die 'Setup cancelled.'
    printf '%s' "${answer:-$2}"
}

prepare_service() {
    [[ $install_dir == /usr/local/bin ]] || die 'Custom INSTALL_DIR requires --no-service.'
    if ! command -v systemctl >/dev/null || [[ ! -d /run/systemd/system ]]; then
        die 'systemd is required. Use --no-service for a foreground install.'
    fi
    if [[ $EUID -ne 0 ]]; then
        command -v sudo >/dev/null || die 'Install sudo or run as root to set up the service.'
        elevate=(sudo)
        sudo -v
    fi
    service=sn46-validator.service
    local load_state
    load_state=$(systemctl show -p LoadState --value "$service")
    if [[ $load_state == not-found ]]; then
        if [[ ! -r /dev/tty ]] || ! ( : < /dev/tty ) 2>/dev/null; then
            die 'Fresh service setup needs a terminal. Use --no-service for a foreground install.'
        fi
        local user account wallet_path wallet_name wallet_hotkey
        user=$(prompt 'Linux user who owns the wallet' "${SUDO_USER:-$(id -un)}")
        account=$(getent passwd "$user") || die "Unknown Linux user: $user"
        [[ $user =~ ^[a-zA-Z_][a-zA-Z0-9_-]*\$?$ ]] || die 'Invalid Linux user.'
        wallet_path=$(printf '%s' "$account" | cut -d: -f6)/.bittensor/wallets
        wallet_path=$(prompt 'Wallet directory' "$wallet_path")
        wallet_name=$(prompt 'Wallet name' validator)
        wallet_hotkey=$(prompt 'Hotkey name' default)
        [[ $wallet_path == /* ]] || die 'Wallet directory must be an absolute path.'
        "${elevate[@]}" test -f "$wallet_path/$wallet_name/hotkeys/$wallet_hotkey" ||
            die 'Hotkey file not found. Copy your wallet to this machine before installing.'
        # EnvironmentFile uses quoted strings, not shell evaluation.
        local key value
        for key in WALLET_PATH WALLET_NAME WALLET_HOTKEY; do
            case "$key" in
                WALLET_PATH) value=$wallet_path ;;
                WALLET_NAME) value=$wallet_name ;;
                WALLET_HOTKEY) value=$wallet_hotkey ;;
            esac
            value=${value//\\/\\\\}
            value=${value//\"/\\\"}
            printf '%s="%s"\n' "$key" "$value"
        done > "$work_dir/config"
        cat > "$work_dir/service" <<UNIT
[Unit]
Description=SN46 validator
After=network-online.target
Wants=network-online.target

[Service]
User=$user
EnvironmentFile=/etc/sn46-validator/config
ExecStart=/usr/local/bin/sn46-validator run
StateDirectory=sn46-validator
StateDirectoryMode=0700
UMask=0077
Restart=on-failure
RestartSec=5
TimeoutStopSec=90
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=true

[Install]
WantedBy=multi-user.target
UNIT
    fi
}

start_service() {
    if [[ -f $work_dir/service ]]; then
        "${elevate[@]}" install -d -m 0700 /etc/sn46-validator
        "${elevate[@]}" install -m 0600 "$work_dir/config" /etc/sn46-validator/config
        "${elevate[@]}" install -m 0644 "$work_dir/service" "/etc/systemd/system/$service"
    fi
    # Preserve existing wallet/network settings and state when upgrading a service.
    printf '[Service]\nExecStart=\nExecStart=/usr/local/bin/sn46-validator run\n' > "$work_dir/override.conf"
    "${elevate[@]}" install -d -m 0755 "/etc/systemd/system/$service.d"
    "${elevate[@]}" install -m 0644 "$work_dir/override.conf" "/etc/systemd/system/$service.d/99-sn46-validator.conf"
    "${elevate[@]}" systemctl daemon-reload
    "${elevate[@]}" systemctl enable "$service"
    "${elevate[@]}" systemctl restart "$service"
    "${elevate[@]}" systemctl is-active --quiet "$service" || die "Service failed to start; check journalctl -u $service."
    printf '\n✅ %s started; it will restart after reboot.\n' "$service"
    printf 'Logs: sudo journalctl -u %s -f -o cat\n' "$service"
}

main() {
    version=
    with_service=true
    for arg in "$@"; do
        case "$arg" in
            --no-service) with_service=false ;;
            v*) [[ -z $version ]] || die 'Specify only one version.'; version=$arg ;;
            *) die 'Usage: install.sh [vX.Y.Z] [--no-service]' ;;
        esac
    done
    [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] ||
        die 'This release supports Linux x86_64 (Ubuntu 22.04 or newer).'

    repository=https://github.com/Subnet46/sn46-validator
    if [[ -z $version ]]; then
        release_url=$(curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --connect-timeout 15 \
            --output /dev/null --write-out '%{url_effective}' "$repository/releases/latest")
        version=${release_url##*/}
    fi
    [[ $version =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die 'Expected a release version such as v0.1.0.'

    asset=sn46-validator-linux-x86_64
    work_dir=$(mktemp -d)
    staged_path=
    elevate=()
    trap 'rm -rf -- "$work_dir"; if [[ -n $staged_path ]]; then "${elevate[@]}" rm -f -- "$staged_path"; fi' EXIT
    for file in "$asset" SHA256SUMS; do
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --connect-timeout 15 \
            --output "$work_dir/$file" "$repository/releases/download/$version/$file"
    done
    read -r digest checksum_file < "$work_dir/SHA256SUMS"
    [[ $digest =~ ^[a-f0-9]{64}$ && $checksum_file == "$asset" ]] || die 'Missing binary checksum.'
    (cd "$work_dir" && sha256sum --check --status SHA256SUMS) || die 'Checksum verification failed.'
    chmod 0755 "$work_dir/$asset"
    binary_version=$("$work_dir/$asset" --version) || die 'The binary cannot run on this machine.'
    [[ $binary_version == "sn46-validator ${version#v}" ]] || die 'Binary version does not match the release.'

    install_dir=${INSTALL_DIR:-/usr/local/bin}
    if $with_service; then prepare_service; fi
    if ! mkdir -p -- "$install_dir" 2>/dev/null || [[ ! -w $install_dir ]]; then
        command -v sudo >/dev/null || die 'Set INSTALL_DIR to a writable directory or install sudo.'
        elevate=(sudo)
        sudo -v
        sudo mkdir -p -- "$install_dir"
    fi
    staged_path=$("${elevate[@]}" mktemp "$install_dir/.sn46-validator.XXXXXX")
    "${elevate[@]}" install -m 0755 -- "$work_dir/$asset" "$staged_path"
    "${elevate[@]}" mv -f -- "$staged_path" "$install_dir/sn46-validator"
    staged_path=
    printf 'Installed %s at %s/sn46-validator\n' "$binary_version" "$install_dir"
    if $with_service; then
        start_service
        return
    fi
    printf '\nCommands:\n'
    printf '  %s/sn46-validator run [options]       Run continuously\n' "$install_dir"
    printf '  %s/sn46-validator run-once [options]  Run once and exit\n' "$install_dir"
    printf '  %s/sn46-validator --help              Show all options\n' "$install_dir"
}

main "$@"
