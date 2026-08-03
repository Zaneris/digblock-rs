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

## Operating

- **Upgrade:** `docker compose pull && docker compose up -d`
- **Backup:** the `./data` directory (one journal file per world seed)
- **New world:** set `SEED` in `.env` and restart; each seed keeps its
  own journal, so switching back restores the old world
- Certificates and master registration are fully automatic (the server
  mints and rotates its own cert and re-registers as needed)
