#!/usr/bin/env bash
# Sign a CI-built validator release with the offline validator key (never the miner key).
#
#   scripts/sign-release.sh v0.1.4 [--apply-after 2026-10-02T02:00:00Z | 1790000000000] [--sequence N]
#
# Downloads the draft release's binary, checks it against SHA256SUMS and its --version (run
# in a bubblewrap sandbox that cannot see the signing key, credentials or network),
# refuses a sequence at or below the published latest manifest's, writes a canonical
# manifest.json, signs it with sn46-release-sign and prints the upload commands. It never
# uploads or publishes anything itself.
set -euo pipefail

die() { printf 'sign-release: %s\n' "$*" >&2; exit 1; }
usage() { die 'Usage: scripts/sign-release.sh vX.Y.Z [--apply-after RFC3339|epoch-ms] [--sequence N]'; }

repository=Subnet46/sn46-validator
asset=sn46-validator-linux-x86_64
repository_dir=$(cd -- "$(dirname -- "$0")/.." && pwd)
keys_dir=${RELEASE_KEYS_DIR:-$repository_dir/../release-keys}
secret_key=$keys_dir/validator.secret
signer=${SN46_RELEASE_SIGN:-$keys_dir/sn46-release-sign}

[[ $# -ge 1 ]] || usage
tag=$1
shift
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "expected a tag such as v0.1.4, got $tag"
version=${tag#v}
apply_after_ms=$(date -u +%s%3N)
sequence=
while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply-after)
            [[ $# -ge 2 ]] || usage
            if [[ $2 =~ ^[0-9]+$ ]]; then
                apply_after_ms=$2
            else
                seconds=$(date -u -d "$2" +%s) || die "cannot parse --apply-after $2"
                apply_after_ms=$((seconds * 1000))
            fi
            shift 2 ;;
        --sequence)
            [[ $# -ge 2 && $2 =~ ^[0-9]+$ ]] || usage
            sequence=$2
            shift 2 ;;
        *) usage ;;
    esac
done

[[ -f $secret_key ]] || die "missing $secret_key"
[[ -x $signer ]] || die "missing $signer (build sn46-release-sign from sn46-workers or set SN46_RELEASE_SIGN)"
command -v gh >/dev/null || die 'gh is required'
command -v bwrap >/dev/null || die 'bwrap (bubblewrap) is required to run the unsigned binary'

work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT

# Draft releases are visible to gh with the owner's credentials.
gh release download "$tag" --repo "$repository" --dir "$work_dir" \
    --pattern "$asset" --pattern SHA256SUMS
(cd "$work_dir" && sha256sum --check --status SHA256SUMS) || die 'SHA256SUMS does not match the binary'
chmod 0755 "$work_dir/$asset"
if [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]]; then
    # The binary is unsigned until we sign it: run it with only /usr and itself visible, so
    # it cannot read the signing key, gh credentials or anything else of ours.
    reported=$(bwrap --unshare-all --die-with-parent --new-session --clearenv \
        --ro-bind /usr /usr --ro-bind-try /lib /lib --ro-bind-try /lib64 /lib64 \
        --ro-bind-try /bin /bin --proc /proc --dev /dev --tmpfs /tmp \
        --ro-bind "$work_dir/$asset" /tmp/candidate -- /tmp/candidate --version) ||
        die 'the binary does not run'
    [[ $reported == "sn46-validator $version" ]] || die "binary reports '$reported', expected sn46-validator $version"
fi
sha256=$(sha256sum "$work_dir/$asset" | cut -d' ' -f1)
size=$(stat -c %s "$work_dir/$asset")

# The published latest manifest's sequence; no manifest yet counts as 0.
status=$(curl --proto '=https' --tlsv1.2 -sSL --retry 3 --connect-timeout 15 \
    --output "$work_dir/published.json" --write-out '%{http_code}' \
    "https://github.com/$repository/releases/latest/download/manifest.json") ||
    die 'cannot fetch the published manifest'
case "$status" in
    200)
        published=$(grep -o '"sequence":[0-9]*' "$work_dir/published.json" | cut -d: -f2)
        [[ $published =~ ^[0-9]+$ ]] || die 'the published manifest has no sequence' ;;
    404) published=0 ;;
    *) die "fetching the published manifest returned HTTP $status" ;;
esac
sequence=${sequence:-$((published + 1))}
((sequence > published)) || die "sequence $sequence is not above the published sequence $published"

out_dir=$repository_dir/dist/$tag
mkdir -p "$out_dir"
rm -f -- "$out_dir/manifest.json" "$out_dir/manifest.sig"
# Canonical compact JSON, keys in byte order, one trailing newline: the bytes that are signed.
printf '{"apply_after_ms":%s,"binary":{"name":"%s","sha256":"%s","size":%s},"schema":"sn46.validator.release.v1","sequence":%s,"version":"%s"}\n' \
    "$apply_after_ms" "$asset" "$sha256" "$size" "$sequence" "$version" > "$out_dir/manifest.json"
"$signer" --manifest "$out_dir/manifest.json" --secret-key "$secret_key" --output "$out_dir/manifest.sig"

printf '\nSigned %s: sequence %s (published %s), apply after %s ms\n' "$tag" "$sequence" "$published" "$apply_after_ms"
cat "$out_dir/manifest.json"
printf '\nReview, then upload and publish:\n'
printf '  gh release upload %s --repo %s %q %q\n' "$tag" "$repository" "$out_dir/manifest.json" "$out_dir/manifest.sig"
printf '  gh release edit %s --repo %s --draft=false --latest\n' "$tag" "$repository"
