"""`python3 -m unittest tools/fleet/fleet_test.py` — no network, no hosts."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import fleet  # noqa: E402

FULL = fleet.Values(port=4434, tag="0.8.2", name="Canada East", seed="7", admins=("id-one", "id-two"), advertise_host="play.example.com")
BARE = fleet.Values(port=4433, tag="0.8.2", name="Plain")


def inventory(**overrides):
	data = {
		"version": "0.8.2",
		"hosts": {"a": {"addr": "203.0.113.1", "user": "debian"}},
		"servers": {"a1": {"host": "a", "dir": "digblock", "port": 4433, "name": "One"}},
	}
	data["servers"].update(overrides)
	return data


class RenderParse(unittest.TestCase):
	def test_round_trip_with_every_optional_on_and_off(self):
		for values in (FULL, BARE):
			with self.subTest(values=values):
				self.assertEqual(fleet.parse(fleet.render(values)), values)

	def test_rendering_the_kit_twice_is_stable(self):
		once = fleet.render(FULL)
		self.assertEqual(fleet.render(FULL, kit=once), once)

	def test_an_absent_optional_keeps_the_kits_commented_example(self):
		kit = fleet.KIT.read_text()
		rendered = fleet.render(BARE)
		for key in fleet.OPTIONAL:
			kit_line = next(line for line in kit.splitlines() if key in line and line.lstrip().startswith("#"))
			self.assertIn(kit_line, rendered)

	def test_the_kit_itself_parses(self):
		values = fleet.parse(fleet.KIT.read_text())
		self.assertEqual(values.port, fleet.PORT_BLOCK[0])
		self.assertEqual(values.tag, "latest")
		self.assertIsNone(values.seed)
		self.assertEqual(values.admins, ())

	def test_a_file_missing_a_required_slot_is_refused(self):
		broken = "\n".join(line for line in fleet.KIT.read_text().splitlines() if "DIGBLOCK_SERVER_NAME" not in line)
		with self.assertRaises(fleet.Fatal):
			fleet.parse(broken)

	def test_a_kit_missing_any_slot_cannot_render(self):
		broken = "\n".join(line for line in fleet.KIT.read_text().splitlines() if "DIGBLOCK_SEED" not in line)
		with self.assertRaises(fleet.Fatal):
			fleet.render(FULL, kit=broken)

	def test_a_file_from_an_older_kit_still_parses(self):
		"""A host may hold a file pushed before a newer optional slot existed (the world levers,
		say); apply has to read that file to replace it, so an absent optional parses as off and
		the next render pushes the fully slotted kit."""
		for key in fleet.OPTIONAL:
			with self.subTest(key):
				old = "\n".join(line for line in fleet.render(BARE).splitlines() if key not in line)
				self.assertEqual(fleet.parse(old), BARE)
		with self.assertRaises(fleet.Fatal, msg="a duplicated optional is still mangled, not old"):
			doubled = fleet.render(BARE) + '\n      #DIGBLOCK_SEED: "42"\n'
			fleet.parse(doubled)

	def test_a_quote_in_a_name_cannot_reach_yaml(self):
		with self.assertRaises(fleet.Fatal):
			fleet.render(fleet.Values(port=4433, tag="x", name='say "hi"'))

	def test_the_world_levers_round_trip_and_default_off(self):
		"""Two switches, never one: a row may arm either without arming the other, and a row that
		has decided nothing pushes neither. Asserted on WHOLE LINES, because the kit carries a
		commented example of each and a substring test would pass on that."""

		def armed_in(text: str, key: str) -> bool:
			want = f'DIGBLOCK_{key.upper()}: "true"'
			return any(line.strip() == want for line in text.splitlines())

		for key in fleet.DECISIONS:
			with self.subTest(key):
				self.assertFalse(getattr(BARE, key), "off unless a row says otherwise")
				self.assertFalse(armed_in(fleet.render(BARE), key), "an undecided row pushes it")
				armed = fleet.Values(port=4433, tag="x", name="n", **{key: True})
				text = fleet.render(armed)
				self.assertEqual(fleet.parse(text), armed, "round trip")
				self.assertTrue(armed_in(text, key))
				other = next(k for k in fleet.DECISIONS if k != key)
				self.assertFalse(armed_in(text, other), "one lever armed the other")

	def test_the_value_summary_ignores_seed_spelling(self):
		a = fleet.Values(port=4433, tag="x", name="n", seed="6")
		b = fleet.Values(port=4433, tag="x", name="n", seed="6.0")
		self.assertEqual(a.changes(b), [])
		self.assertEqual(len(a.changes(fleet.Values(port=4434, tag="y", name="n", seed="7"))), 3)


class InventoryRules(unittest.TestCase):
	def test_a_good_inventory_loads(self):
		inv = fleet.build_inventory(inventory())
		self.assertEqual(inv.servers["a1"].values.tag, "0.8.2")

	def test_a_row_overrides_the_fleet_version(self):
		inv = fleet.build_inventory(inventory(a1={"host": "a", "dir": "digblock", "port": 4433, "name": "One", "version": "0.7.0"}))
		self.assertEqual(inv.servers["a1"].values.tag, "0.7.0")

	def test_refusals(self):
		cases = {
			"port below the block": inventory(a1={"host": "a", "dir": "d", "port": fleet.PORT_BLOCK[0] - 1, "name": "n"}),
			"port above the block": inventory(a1={"host": "a", "dir": "d", "port": fleet.PORT_BLOCK[1] + 1, "name": "n"}),
			"duplicate port on a host": inventory(a2={"host": "a", "dir": "other", "port": 4433, "name": "n"}),
			"duplicate dir on a host": inventory(a2={"host": "a", "dir": "digblock", "port": 4434, "name": "n"}),
			"unknown host": inventory(a2={"host": "zz", "dir": "other", "port": 4434, "name": "n"}),
			"unknown key": inventory(a2={"host": "a", "dir": "other", "port": 4434, "name": "n", "seeed": 1}),
			"nested dir": inventory(a2={"host": "a", "dir": "x/y", "port": 4434, "name": "n"}),
			"admins not a list": inventory(a2={"host": "a", "dir": "other", "port": 4434, "name": "n", "admins": "id"}),
			"a world lever that is not a bool": inventory(a2={"host": "a", "dir": "other", "port": 4434, "name": "n", "archive_world": "yes"}),
		}
		for label, data in cases.items():
			with self.subTest(label), self.assertRaises(fleet.Fatal):
				fleet.build_inventory(data)
		self.assertEqual(len(cases), 9)

	def test_adopt_output_reloads_to_the_same_inventory(self):
		inv = fleet.build_inventory(inventory(a2={"host": "a", "dir": "digblock2", "port": 4434, "name": "Two", "seed": 7, "admins": ["id"], "version": "0.7.0", "adopt_terrain": True}))
		text = fleet.toml_rows(inv, {n: (s.host, s.dir, s.values) for n, s in inv.servers.items()})
		again = fleet.build_inventory(fleet.tomllib.loads(text))
		self.assertEqual(again, inv)

	def test_selection_by_server_or_host(self):
		inv = fleet.build_inventory(inventory(a2={"host": "a", "dir": "digblock2", "port": 4434, "name": "Two"}))
		self.assertEqual([s.name for s in inv.select([])], ["a1", "a2"])
		self.assertEqual([s.name for s in inv.select(["a"])], ["a1", "a2"])
		self.assertEqual([s.name for s in inv.select(["a2", "a"])], ["a2", "a1"])
		with self.assertRaises(fleet.Fatal):
			inv.select(["nope"])


class LogClassifier(unittest.TestCase):
	def test_each_verdict(self):
		self.assertEqual(fleet.classify("registered with master https://m as id (listed at h:4433)"), "live")
		self.assertEqual(fleet.classify("master says we are UNREACHABLE (timeout) — clients cannot join."), "failed")
		self.assertEqual(fleet.classify("failed to bind udp 4433: in use"), "failed")
		self.assertEqual(fleet.classify("archiving /data/world-x.journal to /data/world-x.v1.bak"), "archived")
		self.assertEqual(fleet.classify("world archived; starting fresh under the current generator"), "archived")
		self.assertIsNone(fleet.classify("listening on udp 4433"))

	def test_a_refusal_is_never_a_successful_boot(self):
		"""The server logs a REFUSAL containing "registered with master"; a player reconnecting
		during an upgrade would otherwise end the watch with a verdict of "live"."""
		self.assertIsNone(fleet.classify("conn 3: refused (server not yet registered with master)"))

	def test_the_boot_refusal_is_a_failure(self):
		"""What `Journal::open` refusing a stale format actually reaches the log as."""
		line = (
			"failed to open world: journal version 1 != 3: /data/world-x.journal is from another "
			"record format and was left untouched. Run the build that wrote it, or archive it and "
			"start fresh (the server's --archive-world)"
		)
		self.assertEqual(fleet.classify(line), "failed")


class SeedParsing(unittest.TestCase):
	def test_a_non_numeric_seed_is_refused_not_a_traceback(self):
		bad = fleet.Values(port=4433, tag="x", name="n", seed="not-a-number")
		with self.assertRaises(fleet.Fatal):
			bad.same_world(BARE)


class PsShapes(unittest.TestCase):
	def test_ndjson_and_array(self):
		entry = '{"ID":"abc","State":"running"}'
		self.assertEqual(fleet.ps_entries(entry + "\n"), [{"ID": "abc", "State": "running"}])
		self.assertEqual(fleet.ps_entries("[" + entry + "]\n"), [{"ID": "abc", "State": "running"}])
		self.assertEqual(fleet.ps_entries(""), [])


if __name__ == "__main__":
	unittest.main()
