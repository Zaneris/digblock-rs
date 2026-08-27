# Fleet

`just fleet` drives the dedicated servers in `fleet.toml` over ssh. The inventory is authoritative: each server's
`compose.yml` on its host is `deploy/compose.yml` with the row's values written into the kit's slots, so the kit stays
the one template and a kit change reaches every server on the next `apply`.

## Inventory

A `[hosts.X]` row is an address and a login. A `[servers.Y]` row names its host, the folder under that user's home, the
port (which is also the compose project's name — the kit's `x-port` anchor), the listed name, and optionally `seed`,
`admins`, `advertise_host`, a `version` overriding the fleet-wide one, and the two world-replacement levers
`archive_world` / `adopt_terrain` (see **Update**). Every key is checked on load: an unknown key, a non-boolean lever, a
port outside the master's block, or two rows sharing a host's port or folder is fatal.

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

**Update** is editing `version =` (fleet-wide or on one row) and running `apply`: a tag change stops the server, tars
`./data` into `~/digblock-backups` on the host, pulls, brings it up and watches the boot. **Rollback** is reverting that
edit and applying again. Git history is the deploy log.

A record-format break does NOT replace the world any more — the server refuses to boot and says so, and `apply` reports
that as a failure. Taking the new format is `archive_world = true` on the row (`--archive-world`), which moves the world
aside to `.vN.bak` on the host and starts fresh; `adopt_terrain = true` is the separate lever for giving a readable
world up for a new generator. Both are inert until their own reason applies, and `apply` shouts when either fires. Unset
them once the upgrade is through — leaving one set is safe, but the row should say what is true.

Two changes are refused without `--i-mean-it`: a seed, because it starts a fresh world, and a port, because it renames
the compose project (the old one is torn down first).

`backup --fetch` also copies the archive under `data/fleet-backups/` here, which git ignores. Nothing in this tool takes
a password; after `setup`, every call is `BatchMode`.
