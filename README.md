# sn46-validator

SN46 validator for Linux x86_64 (Ubuntu 22.04+). Defaults to **Finney mainnet, subnet 46**, with our platform URL and signer built in.

## Minimum requirements

- 1 vCPU
- 2 GB RAM
- 50 GB SSD
- Ubuntu 22.04+ (x86_64)
- Stable internet connection
- No GPU required

Our Rust validator runs on a DigitalOcean droplet with these specs.

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
