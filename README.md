# sn46-validator

SN46 validator for Linux x86_64 (Ubuntu 22.04+). Defaults to **Finney mainnet, subnet 46**, with our platform URL and signer built in.

## Minimum requirements

- 1 vCPU
- 2 GB RAM
- 50 GB SSD
- Ubuntu 22.04+ (x86_64)
- Stable internet connection
- No GPU required

## Install

Have your registered wallet on the machine, then run:

```bash
curl -fsSL https://raw.githubusercontent.com/Subnet46/sn46-validator/main/install.sh | bash
```

Select your wallet when prompted. The installer starts the validator as a systemd service and enables it after reboot. Upgrades reuse your existing service settings.

## Logs

```bash
sudo journalctl -u sn46-validator -f -o cat
```

## Run manually

Install with `| bash -s -- --no-service` to skip systemd, then:

```bash
sn46-validator run --wallet-name YOUR_WALLET --wallet-hotkey YOUR_HOTKEY
```

`run` keeps running and prints logs. `run-once` runs once. Use `--help` for options or `--log debug` for details. Stop the service before running manually with the same wallet.

## Releasing

Pushing a `vX.Y.Z` tag builds and tests the binary and publishes it as a **draft** release. Validators only update from a signed release marked latest, so the owner then signs it on the offline machine that holds `release-keys/validator.secret`:

```bash
scripts/sign-release.sh v0.1.4                                      # roll out now (plus each host's spread)
scripts/sign-release.sh v0.1.4 --apply-after 2026-10-02T02:00:00Z   # or start the rollout later
```

The script downloads the draft's binary, checks it against `SHA256SUMS` and its `--version` (run under `bwrap`, which must be installed, with no network and no view of the keys or your home directory), refuses a sequence at or below the published latest manifest's (`--sequence N` overrides the default of published + 1), writes `dist/vX.Y.Z/manifest.json` and signs it into `manifest.sig` with `sn46-release-sign` (set `RELEASE_KEYS_DIR` or `SN46_RELEASE_SIGN` if they are not in `../release-keys`). It uploads nothing; it prints the `gh release upload` and `gh release edit --draft=false --latest` commands to run after review.
