# Telos Reth production operations assets

These files are a reference deployment for one network instance per systemd template (for example,
`mainnet` or `testnet`). The execution client runs as the unprivileged `telos-reth` user. Only the
coordinated snapshot service runs as root because it must stop two units and preserve database
ownership.

| Path | Purpose |
| --- | --- |
| `systemd/telos-reth@.service` | Locked-down execution service; secrets are systemd credentials |
| `systemd/telos-reth-readiness@.*` | JWT-authenticated, canonical-parity readiness every 30 seconds |
| `systemd/telos-reth-snapshot@.*` | Coordinated reflink snapshot plus mandatory remote restic copy |
| `scripts/telos-reth-preflight` | Release digest, configuration, credential, and companion pin checks |
| `scripts/telos-reth-readiness` | Fail-closed health gates and node_exporter textfile metrics |
| `scripts/telos-reth-snapshot` | Atomic checksum manifest and remote verification |
| `scripts/telos-reth-restore` | Verified pair restore with automatic rollback on failed readiness |
| `scripts/telos-reth-release` | Immutable release installation and atomic activation without restart |
| `prometheus/alerts.yml` | Correctness, availability, durability, and capacity alerts |

The complete install, upgrade, rollback, recovery, and incident procedures are in
[`docs/telos/operations.md`](../docs/telos/operations.md). Do not copy the old fork's `/tmp` state-diff
handoff or journal-driven auto-restart daemon. State reconciliation belongs to the same
JWT-authenticated Engine request, and canonical mismatches require quarantine and investigation.

Validate these assets on the target Linux distribution before installation:

```bash
shellcheck ops/scripts/*
systemd-analyze verify ops/systemd/*.service ops/systemd/*.timer
promtool check rules ops/prometheus/alerts.yml
promtool check config ops/prometheus/scrape.example.yml
```
