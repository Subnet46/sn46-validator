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

## Updates

Service installs update themselves. A systemd timer (`sn46-validator-update.timer`, every 15 minutes) runs `sn46-validator update` as root, which installs the latest release only when:

- its `manifest.json` is signed with the validator release key built into the binary (the miner key is never accepted), and its sequence is newer than the one in `/var/lib/sn46-validator-update/release.json`, so an old release can never be replayed;
- the release's apply-after time plus this host's spread has passed. The spread is a fixed delay of up to 2 hours (`UPDATE_SPREAD_S`, default 7200) derived from your hotkey, so validators do not all restart at once;
- the time is inside your maintenance window, if you set one.

Settings go in `/etc/sn46-validator/config` (times are UTC):

```bash
AUTO_UPDATE=0                 # turn automatic updates off
UPDATE_WINDOW="02:00-05:00"   # only update in this window; may wrap midnight, e.g. 22:00-02:00
```

An update downloads the binary next to the installed one, checks its size, SHA-256 and `--version`, keeps the old binary as `/usr/local/bin/sn46-validator.previous`, swaps it in and restarts the service. If the service is not active or restarts on its own within 30 seconds, the updater puts the old binary back, restarts it, and never retries that release (a newer one is tried as usual). If the updater is interrupted after the swap, its next run finishes the health check first. If the disk lacks room for three copies of the binary, the update is skipped with a warning and `/var/lib/sn46-validator-update/update-status.json` says how much space is needed.

Check it:

```bash
systemctl list-timers sn46-validator-update.timer
sudo journalctl -u sn46-validator-update
sudo systemctl start sn46-validator-update     # check now (still honours the schedule)
sudo sn46-validator update --now               # install a due release now, ignoring apply-after and the window
```

Every run logs why it did or did not update. Hosts installed with v0.1.2 or earlier have no timer: run the installer once more to get it.

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
