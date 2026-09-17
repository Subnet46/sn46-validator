#!/usr/bin/env bash
# Exercise downloads and failure handling without network access or root.
set -euo pipefail
repository_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
test_dir=$(mktemp -d)
trap 'rm -rf -- "$test_dir"' EXIT
export MOCK_ASSETS="$test_dir/assets" INSTALL_DIR="$test_dir/bin"
mkdir -p "$MOCK_ASSETS" "$test_dir/tools" "$INSTALL_DIR"
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
    printf 'https://github.com/Subnet46/sn46-validator/releases/tag/v0.1.0'
    exit
fi
[[ $url == */releases/download/v0.1.0/* ]] || exit 22
[[ $MOCK_DOWNLOAD_FAIL == 0 ]] || exit 22
cp "$MOCK_ASSETS/${url##*/}" "$output"
SH
chmod +x "$test_dir/tools/"*
export PATH="$test_dir/tools:$PATH"
if [[ -n ${RELEASE_BINARY:-} ]]; then
    cp "$RELEASE_BINARY" "$MOCK_ASSETS/sn46-validator-linux-x86_64"
else
    printf '#!/usr/bin/env bash\nprintf "sn46-validator 0.1.0\\n"\n' > "$MOCK_ASSETS/sn46-validator-linux-x86_64"
fi
checksum() { (cd "$MOCK_ASSETS" && sha256sum sn46-validator-linux-x86_64 > SHA256SUMS); }
checksum

# Pipe installation, latest resolution, and replacement with a pinned version.
cat "$repository_dir/install.sh" | bash > "$test_dir/output"
[[ $("$INSTALL_DIR/sn46-validator" --version) == 'sn46-validator 0.1.0' ]]
printf 'old binary\n' > "$INSTALL_DIR/sn46-validator"
bash "$repository_dir/install.sh" v0.1.0 > "$test_dir/output"
[[ $("$INSTALL_DIR/sn46-validator" --version) == 'sn46-validator 0.1.0' ]]
cp "$INSTALL_DIR/sn46-validator" "$test_dir/installed"

must_fail() {
    if bash "$repository_dir/install.sh" "$@" > "$test_dir/output" 2>&1; then
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
