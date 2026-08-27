# Host a DigBlock server

Anyone can run a public DigBlock server — it appears automatically in the in-game server list (the master at
master.digblock.net lists every server it can reach). No signup: your first registration claims your address and the key
that proves it lands in `./data/master-key`, which later registrations present. Players sign in with Google, Facebook or
Microsoft at the master; your server never sees their credentials, only the account id and display name the master
vouches for.

## Quickstart

Needs Docker and UDP port 4433 open to the internet. The image is multi-arch (x86_64 + arm64), so a Raspberry Pi 4/5
running a 64-bit OS works too.

```sh
mkdir digblock-server && cd digblock-server
# download compose.yml from the release, then edit it:
# set DIGBLOCK_SERVER_NAME (the rest is optional)
docker compose up -d
docker compose logs -f  # "registered with master" = you're listed
```

You're listed at the public IP this machine registers from; set `DIGBLOCK_ADVERTISE_HOST` in compose.yml to override it
with a DNS name. If the logs say the master reports you UNREACHABLE, UDP 4433 isn't open — check the
port-forward/firewall.

## Several servers on one machine

One folder per server, each with its own `compose.yml` and its own `./data`, and the port is the one line that differs:

```sh
mkdir digblock-second && cd digblock-second
# download compose.yml from the release, set DIGBLOCK_SERVER_NAME as before,
# and change the x-port line at the top to "4434"
docker compose up -d
```

That one value is the port published on this machine, the port the server tells the master it is reachable at, and the
name of the compose project — so two folders can never claim each other's containers, whatever they are called. The
master lists ports 4433–4499 and nothing outside that block: open that block once on the firewall and every server you
add is covered. Each server registers its own address (`host:port`) and keeps its own `master-key`, and every one of
them shows up in the server list.

## Without Docker

Each release also ships standalone binaries (Linux x86_64 and aarch64, both fully static — the latter covers Raspberry
Pi 4/5 on a 64-bit OS — plus Windows x86_64). Same config via flags or `DIGBLOCK_*` env vars:

```sh
./digblock-server-linux-x86_64 --server-name "My World"
# world data lands in ./data; add --advertise-host play.example.com
# to be listed under a DNS name instead of your public IP, and
# --port 4434 --data-dir ./second for a second server on this machine
```

`--port` must sit in the block the master lists (4433–4499); the server refuses to start otherwise. If players reach you
at a different port than the one you bind — a router forwarding one port to another — `--advertise-port` names the one
they reach.

On Windows: `digblock-server-windows-x86_64.exe` with the same flags (allow it through Windows Firewall when prompted,
and forward UDP 4433 on your router). You're responsible for restarts/autostart — on Linux a minimal systemd unit with
`Restart=always` is the usual answer.

## Admin commands

Set `DIGBLOCK_ADMIN_IDS` (or `--admin`, repeatable) to a player's **account id**; that player gets chat commands (open
chat with T). They find their id in the game under Multiplayer → Account → Copy ID.

A display name works too — `--admin YourName` — and is looked up once when the server starts, then stored as the id. Ids
are what the server actually compares, because a display name is something anyone can ask the master for: granting admin
by name meant anybody who read your player list could take your server. Looking a name up at boot is safe (the master
answers, not the player), and it means renaming does not cost you admin.

- `/help` — list commands
- `/tp <x> <z> [n|s|e|w]` — teleport to the topmost block at that column, optionally facing a compass direction (east =
  sunrise); `~` or `~<d>` teleports relative to where you stand
- `/ban <name>` — ban an online player by IP and kick them
- `/ban-list`, `/unban <name-or-ip>` — inspect and undo bans

Bans record the player's account AND their IP, and either one keeps them out — so a new address no longer gets a griefer
back in. They persist in `./data/bans.json` (hand-editable; the server rereads it only at startup), and `/unban` takes a
name, an IP, or an account id.

## Operating

- **Upgrade:** `docker compose pull && docker compose up -d`
- **If an upgrade refuses to start:** an update that changes the save format will not replace your world to get itself
  running — the server stops with a line naming the file it left untouched. Either go back to the version that wrote it
  (`image:` pinned to that release), or set `DIGBLOCK_ARCHIVE_WORLD: "true"` in `compose.yml` to move the world aside to
  a `.vN.bak` next to it and start fresh. `DIGBLOCK_ADOPT_TERRAIN: "true"` is the separate switch for giving a world
  that still works up for newly generated terrain. Back `./data` up first either way.
- **Backup:** the `./data` directory (one journal directory per world seed holding a shard per world region, player
  inventories, bans, and `master-key` — the credential for re-registering your address)
- **New world:** set `DIGBLOCK_SEED` in `compose.yml` and restart; each seed keeps its own journal, so switching back
  restores the old world
- Certificates and master registration are fully automatic (the server mints and rotates its own cert and re-registers
  as needed)
- **If you lose `./data/master-key`:** the server re-claims its address on the next registration, because the master's
  probe dials the registering machine back and passing that is the proof the key would have been. It works whether or
  not the master still lists the old instance — a stale listing lingers for a couple of minutes after a restart, which
  is exactly when a recovery is attempted. If you are migrating machines, stop the old instance first: the new one has
  to be the machine answering at that address
