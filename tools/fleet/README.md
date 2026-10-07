# Fleet

`just fleet` drives the dedicated servers in `fleet.toml` over ssh. The inventory is authoritative: each server's
`compose.yml` on its host is `deploy/compose.yml` with the row's values written into the kit's slots, so the kit stays
the one template and a kit change reaches every server on the next `apply`.

## Inventory

A `[hosts.X]` row is an address and a login. A `[servers.Y]` row names its host, the folder under that user's home, the
port (which is also the compose project's name — the kit's `x-port` anchor), the listed name, and optionally `seed`,
`admins`, `advertise_host`, a `version` overriding the fleet-wide one, the two world-replacement levers `archive_world`
/ `adopt_terrain` (see **Update**), `no_master` (the server runs unlisted and keys players on their own device key; see
the kit's `DIGBLOCK_NO_MASTER`), and `stopped`, which keeps a server down on purpose: `apply`, `start` and `restart`
pass it by, even by name, while its folder and data stay on the host. `adopt` reads compose files, so it cannot see
`stopped`. Every key is checked on load: an unknown key, a non-boolean lever, a port outside the master's block, or two
rows sharing a host's port or folder is fatal.

## The loop

```sh
just fleet setup            # once per host: installs this machine's key (prompts for the password)
just fleet adopt            # what the hosts hold, printed as fleet.toml rows — plus stray projects and folders
just fleet diff             # rendered vs remote: a value summary, then the text diff
just fleet apply [names]    # push and `up -d`, watching the log until the server registers or fails
just fleet status           # container state, running version vs pinned, the master's view
just fleet start|stop|restart|logs|backup
```

A name is a server row or a host; none means every server.

**Update** is `git pull` and `apply`: the release-server workflow moves the fleet-wide `version =` in the same commit
that bumps the crates, so the pin and the image it names cannot drift. A tag change stops the server, tars `./data` into
`~/digblock-backups` on the host, pulls, brings it up and watches the boot. **Rollback** is editing `version =` back
(fleet-wide or on one row, which the workflow never touches) and applying again. Git history is the release log;
`status` says what each host actually runs against its pin.

A record-format break does NOT replace the world any more — the server refuses to boot and says so, and `apply` reports
that as a failure. Taking the new format is `archive_world = true` on the row (`--archive-world`), which moves the world
aside to `.vN.bak` on the host and starts fresh; `adopt_terrain = true` is the separate lever for giving a readable
world up for a new generator. Both are inert until their own reason applies, and `apply` shouts when either fires. Unset
them once the upgrade is through — leaving one set is safe, but the row should say what is true.

Two changes are refused without `--i-mean-it`: a seed, because it starts a fresh world, and a port, because it renames
the compose project (the old one is torn down first).

`backup --fetch` also copies the archive under `data/fleet-backups/` here, which git ignores. Nothing in this tool takes
a password; after `setup`, every call is `BatchMode`.

## Without the master

A `no_master` server is unlisted and keys players on their own device key, so two things the master used to provide move
to the host:

- **The certificate.** Browsers reach a master-free server only under a CA-issued certificate. On the host: install
  `certbot`, copy `cert-hook.sh` to `/usr/local/bin/digblock-cert-hook`, and issue with
  `certbot certonly --standalone -d HOST --deploy-hook /usr/local/bin/digblock-cert-hook` (TCP 80 open). The hook copies
  each issue and renewal into `~/digblock/data/tls`, which the server serves and reloads within the hour.
- **Saves from the master's days**, filed under master account ids. `master_claims.py MASTER_DB OUT_DIR` exports the
  master's live sessions; `master-claims.txt` goes into the server's `data/` (owner-only, it is a key to every save),
  and the server reads it at boot. A returning client presents the session it kept and gets its save, once, where it
  stood. Export after the clients that refresh sessions are gone, and again before the master is retired. A player with
  no session left (cleared storage, another device) is matched by hand in `master-accounts.txt` and moved with the
  server stopped: `docker compose run --rm digblock --data-dir /data --move-save OLD-ID NEW-ID`, the new id from their
  Account screen.
