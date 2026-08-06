# Host a DigBlock server

Anyone can run a public DigBlock server — it appears automatically in the
in-game server list (the master at master.digblock.net lists every server
it can reach; joining is token-based and names are enforced there).

## Quickstart

Needs Docker and UDP port 4433 open to the internet. The image is
multi-arch (x86_64 + arm64), so a Raspberry Pi 4/5 running a 64-bit OS
works too.

```sh
mkdir digblock-server && cd digblock-server
# download compose.yml from the release, then edit it:
# set DIGBLOCK_SERVER_NAME (the rest is optional)
docker compose up -d
docker compose logs -f  # "registered with master" = you're listed
```

You're listed at the public IP this machine registers from; set
`DIGBLOCK_ADVERTISE_HOST` in compose.yml to override it with a DNS name.
If the logs say the master reports you UNREACHABLE, UDP 4433 isn't
open — check the port-forward/firewall.

## Without Docker

Each release also ships standalone binaries (Linux x86_64 and aarch64,
both fully static — the latter covers Raspberry Pi 4/5 on a 64-bit OS —
plus Windows x86_64). Same config via flags or `DIGBLOCK_*` env vars:

```sh
./digblock-server-linux-x86_64 --server-name "My World"
# world data lands in ./data; add --advertise-host play.example.com
# to be listed under a DNS name instead of your public IP
```

On Windows: `digblock-server-windows-x86_64.exe` with the same flags
(allow it through Windows Firewall when prompted, and forward UDP 4433
on your router). You're responsible for restarts/autostart — on Linux a
minimal systemd unit with `Restart=always` is the usual answer.

## Admin commands

Set `DIGBLOCK_ADMIN_NAME` (or `--admin-name`) to a player display name;
whoever joins under that name gets chat commands (open chat with T):

- `/help` — list commands
- `/tp <x> <z> [n|s|e|w]` — teleport to the topmost block at that
  column, optionally facing a compass direction (east = sunrise);
  `~` or `~<d>` teleports relative to where you stand
- `/ban <name>` — ban an online player by IP and kick them
- `/ban-list`, `/unban <name-or-ip>` — inspect and undo bans

Bans persist in `./data/bans.json` (hand-editable; the server rereads
it only at startup). The name check is exact (case-insensitive), so
pick a name you actually join with.

## Operating

- **Upgrade:** `docker compose pull && docker compose up -d`
- **Backup:** the `./data` directory (one journal directory per world
  seed, holding a shard per 256×256-block region)
- **New world:** set `DIGBLOCK_SEED` in `compose.yml` and restart; each
  seed keeps its own journal, so switching back restores the old world
- Certificates and master registration are fully automatic (the server
  mints and rotates its own cert and re-registers as needed)
