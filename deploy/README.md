# Host a DigBlock server

Anyone can run a public DigBlock server — it appears automatically in the
in-game server list (the master at master.digblock.net lists every server
it can reach; joining is token-based and names are enforced there).

## Quickstart

Needs Docker and UDP port 4433 open to the internet.

```sh
mkdir digblock-server && cd digblock-server
# download compose.yml and .env.example from the release, then:
cp .env.example .env    # set SERVER_NAME and ADVERTISE_HOST
docker compose up -d
docker compose logs -f  # "registered with master" = you're listed
```

`ADVERTISE_HOST` is the public DNS name or IP players reach you at. If
the logs say the master reports you UNREACHABLE, UDP 4433 isn't open —
check the port-forward/firewall.

## Without Docker

Each release also ships standalone binaries (Linux x86_64 fully static,
Windows x86_64). Same config via flags or `DIGBLOCK_*` env vars:

```sh
./digblock-server-linux-x86_64 --advertise-host play.example.com \
  --server-name "My World"    # world data lands in ./data
```

On Windows: `digblock-server-windows-x86_64.exe` with the same flags
(allow it through Windows Firewall when prompted, and forward UDP 4433
on your router). You're responsible for restarts/autostart — on Linux a
minimal systemd unit with `Restart=always` is the usual answer.

## Operating

- **Upgrade:** `docker compose pull && docker compose up -d`
- **Backup:** the `./data` directory (one journal file per world seed)
- **New world:** set `SEED` in `.env` and restart; each seed keeps its
  own journal, so switching back restores the old world
- Certificates and master registration are fully automatic (the server
  mints and rotates its own cert and re-registers as needed)
