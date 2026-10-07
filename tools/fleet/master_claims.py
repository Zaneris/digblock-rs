#!/usr/bin/env python3
"""Export what a master-free server needs from the master's database: `master_claims.py MASTER_DB OUT_DIR`.

  master-claims.txt    one line per live session: the first 16 bytes of the token hash the master
                       stored, in hex, then the account. Goes in the server's data dir as is
                       (apps/server/src/master_claims.rs): a returning client's old session then
                       reclaims the save filed under that account.
  master-accounts.txt  each account and its display name, for matching a player whose session is
                       gone to their old save by hand (`digblock-server --move-save`).

Take it after the last client that refreshes sessions is gone (each refresh rotates the token a
client keeps), and again before the master is retired. Both files are keys or personal data:
written owner-only. Stdlib only.
"""

from __future__ import annotations

import os
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path


def owner_only(path: Path, lines: list[str]) -> None:
	fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
	with os.fdopen(fd, "w") as f:
		f.write("\n".join(lines) + "\n")


def export(db: Path, out: Path) -> tuple[int, int]:
	con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
	if con.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
		raise SystemExit(f"{db} fails its integrity check; download it again")
	# Revoked sessions are left out: a rotated token turning up again is the master's own sign
	# of a stolen copy, and a signed-out one was cleared from its client.
	sessions = con.execute("SELECT TokenHash, AccountId FROM Sessions WHERE RevokedReason IS NULL").fetchall()
	accounts = con.execute("SELECT Id, DisplayName FROM Accounts ORDER BY DisplayName").fetchall()
	for token_hash, account in sessions:
		if not isinstance(token_hash, bytes) or len(token_hash) != 32 or not isinstance(account, str):
			raise SystemExit(f"unexpected session row shape: {type(token_hash)} {type(account)}")
	stamp = f"# exported {datetime.now(timezone.utc):%Y-%m-%dT%H:%MZ} from {db.name}"
	out.mkdir(parents=True, exist_ok=True)
	owner_only(out / "master-claims.txt", [stamp, *(f"{h[:16].hex()} {a}" for h, a in sessions)])
	owner_only(out / "master-accounts.txt", [stamp, *(f"{a} {n or '(no name)'}" for a, n in accounts)])
	return len(sessions), len(accounts)


def main(argv: list[str]) -> int:
	if len(argv) != 3:
		print(__doc__.splitlines()[0], file=sys.stderr)
		return 2
	sessions, accounts = export(Path(argv[1]), Path(argv[2]))
	print(f"{sessions} live session(s), {accounts} account(s) -> {argv[2]}")
	return 0


if __name__ == "__main__":
	sys.exit(main(sys.argv))
