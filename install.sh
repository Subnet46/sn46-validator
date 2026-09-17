#!/usr/bin/env bash
set -euo pipefail

die() { printf 'sn46-validator: %s\n' "$*" >&2; exit 1; }

main() {
    [[ $# -le 1 ]] || die 'Usage: install.sh [vX.Y.Z]'
    [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] ||
        die 'This release supports Linux x86_64 (Ubuntu 22.04 or newer).'

    repository=https://github.com/Subnet46/sn46-validator
    version=${1:-}
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
    printf 'Load your validator environment, then run: %s/sn46-validator\n' "$install_dir"
}

main "$@"
