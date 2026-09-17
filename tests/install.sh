#!/usr/bin/env bash
# Exercise downloads and failure handling without network access or root.
set -euo pipefail
repository_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
test_dir=$(mktemp -d)
trap 'rm -rf -- "$test_dir"' EXIT
export MOCK_ASSETS="$test_dir/assets" INSTALL_DIR="$test_dir/bin"
mkdir -p "$MOCK_ASSETS" "$test_dir/tools" "$INSTALL_DIR"
export MOCK_VERSION
MOCK_VERSION=v$(sed -n 's/^version = "\(.*\)"/\1/p' "$repository_dir/Cargo.toml")
export MOCK_ARCH=x86_64 MOCK_OS=Linux MOCK_DOWNLOAD_FAIL=0
cat > "$test_dir/tools/uname" <<'SH'
#!/usr/bin/env bash
case "$1" in -s) echo "$MOCK_OS";; -m) echo "$MOCK_ARCH";; *) exit 1;; esac
SH
cat > "$test_dir/tools/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
output=
for ((i=1; i <= $#; i++)); do
    if [[ ${!i} == --output ]]; then j=$((i+1)); output=${!j}; fi
done
url=${!#}
if [[ $url == */releases/latest ]]; then
    printf 'https://github.com/Subnet46/sn46-validator/releases/tag/%s' "$MOCK_VERSION"
    exit
fi
[[ $url == */releases/download/"$MOCK_VERSION"/* ]] || exit 22
[[ $MOCK_DOWNLOAD_FAIL == 0 ]] || exit 22
cp "$MOCK_ASSETS/${url##*/}" "$output"
SH
chmod +x "$test_dir/tools/"*
export PATH="$test_dir/tools:$PATH"
if [[ -n ${RELEASE_BINARY:-} ]]; then
    cp "$RELEASE_BINARY" "$MOCK_ASSETS/sn46-validator-linux-x86_64"
else
    printf '#!/usr/bin/env bash\nprintf "sn46-validator %s\\n"\n' "${MOCK_VERSION#v}" > "$MOCK_ASSETS/sn46-validator-linux-x86_64"
fi
checksum() { (cd "$MOCK_ASSETS" && sha256sum sn46-validator-linux-x86_64 > SHA256SUMS); }
checksum

# Pipe installation, latest resolution, and replacement with a pinned version.
# shellcheck disable=SC2002 # Exercise the same piped stdin as curl | bash.
cat "$repository_dir/install.sh" | bash -s -- --no-service > "$test_dir/output"
[[ $("$INSTALL_DIR/sn46-validator" --version) == "sn46-validator ${MOCK_VERSION#v}" ]]
printf 'old binary\n' > "$INSTALL_DIR/sn46-validator"
bash "$repository_dir/install.sh" --no-service "$MOCK_VERSION" > "$test_dir/output"
[[ $("$INSTALL_DIR/sn46-validator" --version) == "sn46-validator ${MOCK_VERSION#v}" ]]
cp "$INSTALL_DIR/sn46-validator" "$test_dir/installed"

must_fail() {
    if bash "$repository_dir/install.sh" --no-service "$@" > "$test_dir/output" 2>&1; then
        echo 'Expected installation to fail' >&2; exit 1
    fi
    cmp "$test_dir/installed" "$INSTALL_DIR/sn46-validator"
    [[ -z $(find "$INSTALL_DIR" -name '.sn46-validator.*' -print -quit) ]]
}
MOCK_ARCH=aarch64 must_fail
MOCK_OS=Darwin must_fail
MOCK_DOWNLOAD_FAIL=1 must_fail
must_fail invalid-version
must_fail v9.9.9
cp "$MOCK_ASSETS/SHA256SUMS" "$test_dir/checksum"
sed 's/sn46-validator-linux-x86_64/unrelated-file/' "$test_dir/checksum" > "$MOCK_ASSETS/SHA256SUMS"
must_fail
cp "$test_dir/checksum" "$MOCK_ASSETS/SHA256SUMS"
printf '\ncorrupted download\n' >> "$MOCK_ASSETS/sn46-validator-linux-x86_64"
must_fail
printf '#!/usr/bin/env bash\nexit 1\n' > "$MOCK_ASSETS/sn46-validator-linux-x86_64"
checksum
must_fail
printf '#!/usr/bin/env bash\necho "sn46-validator 0.2.0"\n' > "$MOCK_ASSETS/sn46-validator-linux-x86_64"
checksum
must_fail
echo 'Installer checks passed'

# Run only inside a disposable Docker container: exercises real file installation
# and terminal prompts, with systemctl mocked (no host services or live wallet).
if [[ ${TEST_SYSTEMD:-0} == 1 ]]; then
    [[ -f /.dockerenv && $EUID == 0 ]] || exit 1
    [[ ! -e /etc/systemd/system/sn46-validator.service ]] || exit 1
    unset INSTALL_DIR
    cp "$test_dir/installed" "$MOCK_ASSETS/sn46-validator-linux-x86_64"
    checksum
    mkdir -p /run/systemd/system /root/.bittensor/wallets/validator/hotkeys
    touch /root/.bittensor/wallets/validator/hotkeys/default
    export MOCK_SYSTEMCTL_LOG="$test_dir/systemctl.log"
    cat > "$test_dir/tools/systemctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$MOCK_SYSTEMCTL_LOG"
case "$1" in
    show)
        if [[ -f /etc/systemd/system/${!#} ]]; then echo loaded; else echo not-found; fi ;;
    daemon-reload|enable|restart|is-active) ;;
    *) exit 1 ;;
esac
SH
    chmod +x "$test_dir/tools/systemctl"
    # Piped installer still reads wallet answers from the controlling terminal.
    printf '\n\n\n\n' | script -q -e -c "cat '$repository_dir/install.sh' | bash" /dev/null > "$test_dir/output"
    [[ -f /etc/systemd/system/sn46-validator.service ]]
    [[ $(stat -c %a /etc/sn46-validator/config) == 600 ]]
    grep -Fx 'WALLET_NAME="validator"' /etc/sn46-validator/config
    grep -Fx 'ExecStart=/usr/local/bin/sn46-validator run' /etc/systemd/system/sn46-validator.service
    # Keep the deploy example and fresh installer unit aligned (except the user/group).
    sed '/^User=/d; /^Group=/d' "$repository_dir/deploy/sn46-validator.service" > "$test_dir/expected-unit"
    sed '/^User=/d' /etc/systemd/system/sn46-validator.service > "$test_dir/actual-unit"
    cmp "$test_dir/expected-unit" "$test_dir/actual-unit"
    # Upgrades preserve the existing service's explicit network settings.
    sed -i '/^ExecStart=/i Environment=NETWORK=local NETUID=5' /etc/systemd/system/sn46-validator.service
    cp /etc/systemd/system/sn46-validator.service "$test_dir/service"
    cp /etc/sn46-validator/config "$test_dir/config"
    bash "$repository_dir/install.sh" > "$test_dir/output"
    cmp "$test_dir/service" /etc/systemd/system/sn46-validator.service
    cmp "$test_dir/config" /etc/sn46-validator/config
    grep -Fx 'restart sn46-validator.service' "$MOCK_SYSTEMCTL_LOG"
    grep -Fx 'ExecStart=' /etc/systemd/system/sn46-validator.service.d/99-sn46-validator.conf
    echo 'Systemd installer checks passed'
fi
