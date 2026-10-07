use super::*;
use digblock_core::block::BlockType;
use digblock_core::hotbar::WIRE_TOOL;

fn temp_dir(tag: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("dbinv-{tag}-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	dir
}

/// A distinct account id per test player. The value is arbitrary — what
/// matters is that two different `n` never collide.
fn key(n: u8) -> IdentityKey {
	IdentityKey([n; 16])
}

/// A distinct pre-account device hash. Same shape as [`key`] and
/// deliberately a different function: the two are the same 16 bytes and
/// mean entirely different things, and a test that used one for the
/// other would read as passing.
fn device(n: u8) -> [u8; 16] {
	[0x80 | n; 16]
}

/// A hand-built v1 file — inventory-only records under device hashes, which is what a server that
/// ran before accounts has on disk. Hand-built rather than round-tripped through the writer on
/// purpose: the writer only emits the current version, so a fixture derived from it could never
/// stand in for the format this migration exists to read.
fn legacy_v1_file(players: &[([u8; 16], u32)]) -> Vec<u8> {
	let mut buf = Vec::new();
	buf.extend_from_slice(&MAGIC);
	buf.extend_from_slice(&1u16.to_le_bytes());
	buf.extend_from_slice(&[0, 0]);
	buf.extend_from_slice(&(players.len() as u32).to_le_bytes());
	for (hash, dirt) in players {
		buf.extend_from_slice(hash);
		buf.push(1);
		buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
		buf.extend_from_slice(&dirt.to_le_bytes());
	}
	buf
}

/// A v4 file of whole records under device hashes — the richest thing
/// a pre-account server could have written, for the merge rules that
/// need a position and skills on the old side.
fn legacy_file(players: &[([u8; 16], PlayerRecord)]) -> Vec<u8> {
	let mut buf = Vec::new();
	buf.extend_from_slice(&MAGIC);
	buf.extend_from_slice(&4u16.to_le_bytes());
	buf.extend_from_slice(&[0, 0]);
	buf.extend_from_slice(&(players.len() as u32).to_le_bytes());
	let mut pairs = Vec::new();
	for (hash, rec) in players {
		write_record(&mut buf, hash, rec, &mut pairs);
	}
	buf
}

fn layout() -> PlayerLayout {
	let mut hotbar = [WIRE_EMPTY; HOTBAR_SLOTS];
	hotbar[0] = BlockType::Dirt as u16;
	hotbar[8] = WIRE_TOOL;
	let mut grid = [WIRE_EMPTY; GRID_SLOTS];
	grid[17] = BlockType::Stone as u16;
	PlayerLayout { hotbar, grid }
}

/// A sized layout blob written at explicit widths, so a fixture can stand for a file some OTHER
/// build wrote — narrower or wider than today's grid. The widths are arguments rather than the
/// current constants on purpose: a fixture built from the live constants moves with them, so it
/// could never fail on the resize it exists to guard against. Words past the caller's own layout
/// are `WIRE_EMPTY`.
fn push_sized_layout(buf: &mut Vec<u8>, l: &PlayerLayout, hotbar_len: usize, grid_len: usize) {
	buf.push(LAYOUT_SIZED);
	buf.push(hotbar_len as u8);
	buf.push(grid_len as u8);
	for i in 0..hotbar_len {
		let w = l.hotbar.get(i).copied().unwrap_or(WIRE_EMPTY);
		buf.extend_from_slice(&w.to_le_bytes());
	}
	for i in 0..grid_len {
		let w = l.grid.get(i).copied().unwrap_or(WIRE_EMPTY);
		buf.extend_from_slice(&w.to_le_bytes());
	}
}

/// Eviction is least-recently-USED, not first-inserted. The old policy
/// never refreshed a key on re-`put`, so it walked strictly in join
/// order and threw out the longest-standing players first — including
/// ones online and actively writing, which is exactly who `put` is
/// being called for.
#[test]
fn eviction_spares_the_recently_used() {
	let dir = temp_dir("lru");
	let mut s = PlayerStore::open(&dir).unwrap();
	let veteran = key(1);
	let mut inv = Inventory::default();
	inv.add(BlockType::Dirt, 7);
	s.put(
		veteran,
		PlayerRecord {
			inventory: inv,
			layout: None,
			pos: None,
			last_teleport_tick: None,
			skills: SkillSet::default(),
			spells: Vec::new(),
		},
	);
	// Fill to the cap with strangers, touching the veteran throughout
	// the way an online player's edits would.
	for i in 0..MAX_IDENTITIES {
		let mut secret = [0u8; 16];
		secret[..8].copy_from_slice(&(i as u64).to_le_bytes());
		secret[8] = 0xff; // never collides with the veteran's [1; 16]
		s.put(IdentityKey(secret), PlayerRecord::default());
		if i % 100 == 0 {
			s.put(veteran, s.get(&veteran));
		}
	}
	assert_eq!(s.map.len(), MAX_IDENTITIES, "cap still bounds the file");
	assert_eq!(
		s.get(&veteran).inventory.count(BlockType::Dirt),
		7,
		"the identity that kept being touched survived the churn"
	);
}

/// The interval flush hands off to the writer thread and returns; the
/// bytes land on disk without the caller ever touching the filesystem.
/// That is the whole point — a whole-file fsync is milliseconds, and
/// this used to happen inside `tick_once`.
#[test]
fn the_interval_flush_persists_without_blocking_the_caller() {
	let dir = temp_dir("asyncwrite");
	let id = key(1);
	let path = dir.join("inventories.bin");
	{
		let mut s = PlayerStore::open(&dir).unwrap();
		let mut inv = Inventory::default();
		inv.add(BlockType::Dirt, 7);
		s.put(
			id,
			PlayerRecord {
				inventory: inv,
				layout: None,
				pos: None,
				last_teleport_tick: None,
				skills: SkillSet::default(),
				spells: Vec::new(),
			},
		);
		// Age the flush clock so `maintain` fires this tick.
		s.last_flush = Instant::now() - FLUSH_INTERVAL;
		s.maintain();
		assert!(!s.dirty, "the snapshot was handed to the writer");

		// The write is on another thread — it appears shortly after.
		let deadline = Instant::now() + Duration::from_secs(10);
		while !path.exists() {
			assert!(Instant::now() < deadline, "the writer never wrote");
			std::thread::sleep(Duration::from_millis(5));
		}
	} // drop joins the writer

	let reopened = PlayerStore::open(&dir).unwrap();
	assert_eq!(reopened.get(&id).inventory.count(BlockType::Dirt), 7);
}

/// File order is the LRU ranking, so a restart resumes it instead of
/// resetting to whatever the hash map happens to iterate.
#[test]
fn lru_order_survives_a_reopen() {
	let dir = temp_dir("lruorder");
	let old = key(1);
	let new = key(2);
	{
		let mut s = PlayerStore::open(&dir).unwrap();
		s.put(old, PlayerRecord::default());
		s.put(new, PlayerRecord::default());
		s.sync();
	}
	let s = PlayerStore::open(&dir).unwrap();
	assert!(
		s.used[&old] < s.used[&new],
		"the earlier-used identity is still the older one after reload"
	);
}

#[test]
fn round_trips_across_reopen() {
	let dir = temp_dir("roundtrip");
	let id_a = key(1);
	let id_b = key(2);
	{
		let mut s = PlayerStore::open(&dir).unwrap();
		let mut inv = Inventory::default();
		inv.add(BlockType::Dirt, 7);
		inv.add(BlockType::Water, 2); // the bucket's charge: caps at one
		s.put(
			id_a,
			PlayerRecord {
				inventory: inv,
				layout: Some(layout()),
				pos: None,
				last_teleport_tick: None,
				skills: SkillSet::default(),
				spells: Vec::new(),
			},
		);
		s.put(id_b, PlayerRecord::default());
		s.sync();
	}
	let s = PlayerStore::open(&dir).unwrap();
	let a = s.get(&id_a);
	assert_eq!(a.inventory.count(BlockType::Dirt), 7);
	assert!(a.inventory.bucket_full());
	assert_eq!(a.inventory.count(BlockType::Water), 1);
	assert_eq!(a.layout, Some(layout()));
	let b = s.get(&id_b);
	assert_eq!(b.inventory, Inventory::default());
	assert_eq!(b.layout, None);
	assert!(s.get(&key(3)).layout.is_none());
}

/// A pre-account file loads into QUARANTINE, not into the account map and not into the archive.
/// Both halves matter: reading those keys as account ids would hand a stranger's inventory to
/// whichever account id shared the bytes, and archiving them deletes every pre-account character
/// on the server. Quarantine is the third option — the records are there, owned by nobody, until
/// the one player who can still prove they owned that device claims them.
#[test]
fn a_pre_account_file_lands_in_quarantine() {
	let dir = temp_dir("v4upgrade");
	std::fs::create_dir_all(&dir).unwrap();
	let path = dir.join("inventories.bin");
	std::fs::write(&path, legacy_v1_file(&[(device(5), 7)])).unwrap();

	let s = PlayerStore::open(&dir).unwrap();
	assert_eq!(s.legacy_len(), 1, "held for a claim");
	assert_eq!(
		s.get(&IdentityKey(device(5))).inventory,
		Inventory::default(),
		"an old record must not be handed to the account whose id shares its bytes"
	);
	assert!(
		!path.with_extension(format!("bin.v{VERSION}.bak")).exists(),
		"an upgrade must never take the archive path"
	);
}

/// EVERY version this project has written still reads. v5 briefly
/// refused the older ones, which would have deleted every pre-account
/// character the first time a server restarted; the quarantine exists
/// so it does not have to.
#[test]
fn every_format_version_still_reads() {
	use digblock_core::skills::Skill;
	for version in 1u16..=VERSION {
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&MAGIC);
		bytes.extend_from_slice(&version.to_le_bytes());
		bytes.extend_from_slice(&[0, 0]);
		bytes.extend_from_slice(&1u32.to_le_bytes());
		bytes.extend_from_slice(&device(7));
		// Two essence pairs — a common balance and an Aether one — so
		// the §14 reset below has something to bite on, and one pair on
		// a retired crest's id, which the raw shapes reuse.
		bytes.push(3);
		bytes.extend_from_slice(&digblock_core::inventory::SUB_WIRE_FLAG.to_le_bytes());
		bytes.extend_from_slice(&500u32.to_le_bytes());
		bytes.extend_from_slice(&(digblock_core::inventory::SUB_WIRE_FLAG | 4).to_le_bytes());
		bytes.extend_from_slice(&7u32.to_le_bytes());
		bytes.extend_from_slice(
			&digblock_core::block::BlockType::REUSED_CREST_IDS
				.start()
				.to_le_bytes(),
		);
		bytes.extend_from_slice(&1u32.to_le_bytes());
		if version >= 2 {
			bytes.push(LAYOUT_ABSENT);
		}
		if version >= 3 {
			bytes.push(0); // no position
			bytes.push(0); // no return tick
		}
		if version >= 4 {
			// A career in two skills, so the Arcana wind-back below has
			// something to bite on and Mining can prove it was spared.
			bytes.push(2);
			for (skill, total) in [
				(Skill::Mining as u8, 146_751u64),
				(Skill::Arcana as u8, 971),
			] {
				bytes.push(skill);
				bytes.extend_from_slice(&total.to_le_bytes());
			}
		}
		if (FIRST_ARRIVALS_VERSION..FIRST_VERSION_WITHOUT_ARRIVALS).contains(&version) {
			bytes.push(0); // no visited waystones
		}
		if version >= FIRST_SPELLS_VERSION {
			bytes.push(0); // empty spell bar
		}
		if version >= FIRST_TWO_SECTION_VERSION {
			bytes.extend_from_slice(&0u32.to_le_bytes()); // empty quarantine
		}
		let file = parse(&bytes).unwrap_or_else(|e| panic!("v{version} must read: {e}"));
		// And it lands on the right side of the split, which is the
		// whole reason the version is still consulted.
		let (accounts, legacy) = (file.accounts.len(), file.legacy.len());
		if version >= FIRST_ACCOUNT_VERSION {
			assert_eq!((accounts, legacy), (1, 0), "v{version} is account-keyed");
		} else {
			assert_eq!((accounts, legacy), (0, 1), "v{version} is device-keyed");
		}
		// The bucket migration: a record from before the vessel was
		// crafted owned one implicitly, and the reader writes that
		// down; a current record owns exactly what its counts say —
		// this fixture's says nothing.
		let record = file
			.accounts
			.iter()
			.map(|(_, r)| r)
			.chain(file.legacy.iter().map(|(_, r)| r));
		for r in record {
			let owned = r.inventory.count(digblock_core::block::BlockType::Bucket);
			if version < FIRST_CRAFTED_BUCKET_VERSION {
				assert_eq!(owned, 1, "v{version} records keep their bucket");
			} else {
				assert_eq!(owned, 0, "v{version} records own what they stored");
			}
			// §14's essence reset: balances from before the redesign
			// clear — all but Aether — and records at the current
			// version keep exactly what they stored.
			use digblock_core::block::Essence;
			let flora = r.inventory.sub_count(Essence::Vitae);
			let aether = r.inventory.sub_count(Essence::Aether);
			if version < FIRST_FOUND_ESSENCE_VERSION {
				assert_eq!(flora, 0, "v{version}: pre-redesign balances clear");
			} else {
				assert_eq!(flora, 500, "v{version}: current records keep theirs");
			}
			assert_eq!(aether, 7, "v{version}: Aether survives either way");
			// The crest purge: a record from before the raw shapes reused
			// the crests' ids carried a crest there, and it drops; a record
			// written since really does hold the slab.
			let slab = r.inventory.count(digblock_core::block::BlockType::DirtSlab);
			if version < FIRST_RAW_SHAPES_VERSION {
				assert_eq!(slab, 0, "v{version}: a crest id drops on read");
			} else {
				assert_eq!(slab, 1, "v{version}: the raw shape reads as itself");
			}
			// The Arcana wind-back: a total earned while first arrivals
			// paid a milestone is not a total this ladder can produce,
			// so it goes to nothing — and takes no other skill with it.
			if version >= 4 {
				let arcana = r.skills.xp(Skill::Arcana);
				if version < FIRST_VERSION_WITHOUT_ARRIVALS {
					assert_eq!(arcana, 0, "v{version}: Arcana winds back");
				} else {
					assert_eq!(arcana, 971, "v{version}: current records keep theirs");
				}
				assert_eq!(
					r.skills.xp(Skill::Mining),
					146_751,
					"v{version}: only Arcana winds back"
				);
			}
		}
	}
}

/// The claim: one player's old character folded into their account, and
/// the source gone. Doing it twice is not an error and not a duplicate
/// — the second attempt finds nothing, exactly as it would on a server
/// this player never played on.
#[test]
fn a_claim_moves_the_character_once_and_only_once() {
	use digblock_core::skills::Skill;
	let dir = temp_dir("claim");
	std::fs::create_dir_all(&dir).unwrap();
	std::fs::write(
		dir.join("inventories.bin"),
		legacy_v1_file(&[(device(1), 7), (device(2), 3)]),
	)
	.unwrap();
	let mut s = PlayerStore::open(&dir).unwrap();
	assert_eq!(s.legacy_len(), 2);

	let account = key(40);
	let got = s.claim_legacy(device(1), account).expect("claimed");
	assert_eq!(got.inventory.count(BlockType::Dirt), 7, "the stuff moved");
	assert_eq!(s.get(&account).inventory.count(BlockType::Dirt), 7);
	assert_eq!(s.legacy_len(), 1, "the source is gone, not copied");

	// Again, with the same key: nothing left to take, and the account's
	// stock is unchanged rather than doubled.
	assert!(s.claim_legacy(device(1), account).is_none());
	assert_eq!(s.get(&account).inventory.count(BlockType::Dirt), 7);

	// A key that was never here answers the same way — which is what
	// every client that keeps its old identity file sends forever.
	assert!(s.claim_legacy(device(99), account).is_none());

	// The OTHER player's character is untouched by all of that, and is
	// still claimable by whoever holds its device.
	let other = key(41);
	assert_eq!(
		s.claim_legacy(device(2), other)
			.expect("still there")
			.inventory
			.count(BlockType::Dirt),
		3
	);
	assert_eq!(s.legacy_len(), 0);
	assert_eq!(s.get(&other).skills.xp(Skill::Mining), 0);
}

/// A claim MERGES rather than overwrites, because the account may
/// already have played here and a player who owns both piles should not
/// have to choose. Each rule is asserted on the side it decides.
#[test]
fn a_claim_merges_into_what_the_account_already_had() {
	use digblock_core::skills::Skill;
	let dir = temp_dir("claimmerge");
	std::fs::create_dir_all(&dir).unwrap();
	let mut old = PlayerRecord {
		inventory: Inventory::default(),
		layout: Some(layout()),
		pos: Some([1.0, 2.0, 3.0]),
		last_teleport_tick: Some(11),
		skills: SkillSet::default(),
		spells: Vec::new(),
	};
	old.inventory.add(BlockType::Dirt, 5);
	old.inventory.add(BlockType::Stone, 9);
	old.skills.add_xp(Skill::Mining, 500);
	old.skills.add_xp(Skill::Alchemy, 10);
	std::fs::write(
		dir.join("inventories.bin"),
		legacy_file(&[(device(3), old)]),
	)
	.unwrap();
	let mut s = PlayerStore::open(&dir).unwrap();

	// The account has been played on this server already.
	let account = key(42);
	let mut mine = PlayerRecord {
		pos: Some([40.0, 50.0, 60.0]),
		last_teleport_tick: Some(77),
		..Default::default()
	};
	mine.inventory.add(BlockType::Dirt, 2);
	mine.skills.add_xp(Skill::Mining, 100);
	s.put(account, mine);

	let got = s.claim_legacy(device(3), account).expect("claimed");
	assert_eq!(got.inventory.count(BlockType::Dirt), 7, "stuff adds up");
	assert_eq!(got.inventory.count(BlockType::Stone), 9, "and carries over");
	assert_eq!(
		got.skills.xp(Skill::Mining),
		500,
		"XP takes the higher total, never the sum"
	);
	assert_eq!(got.skills.xp(Skill::Alchemy), 10, "and the other side too");
	assert_eq!(
		got.pos,
		Some([40.0, 50.0, 60.0]),
		"a claim must not teleport a player who is standing somewhere"
	);
	assert_eq!(
		got.last_teleport_tick,
		Some(77),
		"nor hand them a fresh return by inheriting a stale cooldown"
	);
}

/// The quarantine survives a flush. It is the half nothing touches, so
/// a writer that only wrote the live map would drop every unclaimed
/// character on the first save after boot — silently, and for good.
#[test]
fn the_quarantine_survives_a_save_and_reopen() {
	let dir = temp_dir("quarantineround");
	std::fs::create_dir_all(&dir).unwrap();
	std::fs::write(
		dir.join("inventories.bin"),
		legacy_v1_file(&[(device(1), 7), (device(2), 3)]),
	)
	.unwrap();
	{
		let mut s = PlayerStore::open(&dir).unwrap();
		// An ordinary write by an unrelated player: this is what
		// rewrites the file, and what used to lose the quarantine.
		s.put(key(50), PlayerRecord::default());
		s.sync();
	}
	let mut s = PlayerStore::open(&dir).unwrap();
	assert_eq!(s.legacy_len(), 2, "both still claimable after a rewrite");
	assert_eq!(
		s.claim_legacy(device(2), key(51))
			.expect("survived as itself")
			.inventory
			.count(BlockType::Dirt),
		3
	);

	// And a claim is durable across a restart, which is the other half
	// of one-time-ness: a server bounce must not resurrect the source.
	s.sync();
	let s = PlayerStore::open(&dir).unwrap();
	assert_eq!(s.legacy_len(), 1);
	assert_eq!(s.get(&key(51)).inventory.count(BlockType::Dirt), 3);
}

#[test]
fn corrupt_file_is_archived_and_store_starts_fresh() {
	let dir = temp_dir("corrupt");
	std::fs::create_dir_all(&dir).unwrap();
	let path = dir.join("inventories.bin");
	std::fs::write(&path, b"garbage not a store").unwrap();
	let s = PlayerStore::open(&dir).unwrap();
	assert_eq!(s.get(&key(0)).inventory, Inventory::default());
	let bak = path.with_extension(format!("bin.v{VERSION}.bak"));
	assert_eq!(
		std::fs::read(&bak).unwrap(),
		b"garbage not a store",
		"archived byte-intact"
	);
}

/// A record with no position stored (a player who never disconnected)
/// reads back absent rather than as some default coordinate.
#[test]
fn absent_position_stays_absent() {
	let dir = temp_dir("v3nopos");
	let id = key(10);
	{
		let mut s = PlayerStore::open(&dir).unwrap();
		s.put(
			id,
			PlayerRecord {
				inventory: Inventory::default(),
				layout: None,
				pos: None,
				last_teleport_tick: None,
				skills: SkillSet::default(),
				spells: Vec::new(),
			},
		);
		s.sync();
	}
	let s = PlayerStore::open(&dir).unwrap();
	assert_eq!(s.get(&id).pos, None);
	assert_eq!(s.get(&id).last_teleport_tick, None);
}

/// THE grid-growth test: a complete file written when the grid was narrower still reads, with
/// every player's items, arrangement, position and XP intact — no version bump, no archive path.
/// Growing `GRID_SLOTS` broke exactly this once: a fixed, length-free layout blob let a wider
/// reader over-read the rest of the record, and `open` took the parse error for corruption. The
/// other round-trip fixtures are built from the CURRENT constants; this one declares its widths.
#[test]
fn a_file_written_at_the_old_grid_width_still_reads() {
	use digblock_core::skills::Skill;
	let dir = temp_dir("gridgrowth");
	std::fs::create_dir_all(&dir).unwrap();
	let path = dir.join("inventories.bin");
	let id_a = key(21);
	let id_b = key(22);
	let mut skills = SkillSet::default();
	skills.add_xp(Skill::Mining, 146_751);
	skills.add_xp(Skill::Alchemy, 394);
	// A grid narrower than today's, standing for the build that wrote
	// the file. Derived from the live constant so it stays narrow as the
	// grid grows, and asserted to still contain the slot under test —
	// a fixture that quietly stopped covering its own case would pass
	// forever.
	let narrow = GRID_SLOTS - 9;
	assert!(narrow > 17, "the fixture's occupied slot must be in range");

	// Two records, so an over-read desyncs into the second one's key the
	// way it would on a real server.
	let mut buf = Vec::new();
	buf.extend_from_slice(&MAGIC);
	buf.extend_from_slice(&VERSION.to_le_bytes());
	buf.extend_from_slice(&[0, 0]);
	buf.extend_from_slice(&2u32.to_le_bytes());
	for (id, dirt) in [(id_a, 12u32), (id_b, 5)] {
		buf.extend_from_slice(&id.0);
		buf.push(1);
		buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
		buf.extend_from_slice(&dirt.to_le_bytes());
		push_sized_layout(&mut buf, &layout(), HOTBAR_SLOTS, narrow);
		buf.push(1); // position
		for c in [8.5f32, 133.0, -4.5] {
			buf.extend_from_slice(&c.to_le_bytes());
		}
		buf.push(1); // return tick
		buf.extend_from_slice(&99u32.to_le_bytes());
		let wire = skills.to_wire();
		buf.push(wire.len() as u8);
		for (skill, total) in &wire {
			buf.push(*skill);
			buf.extend_from_slice(&total.to_le_bytes());
		}
		buf.push(0); // empty spell bar (v11+)
	}
	buf.extend_from_slice(&0u32.to_le_bytes()); // empty quarantine
	std::fs::write(&path, &buf).unwrap();

	let mut s = PlayerStore::open(&dir).unwrap();
	assert!(
		!path.with_extension(format!("bin.v{VERSION}.bak")).exists(),
		"a routine grid resize must never take the archive path"
	);
	let a = s.get(&id_a);
	assert_eq!(a.inventory.count(BlockType::Dirt), 12, "items survive");
	assert_eq!(a.pos, Some([8.5, 133.0, -4.5]), "position survives");
	assert_eq!(a.last_teleport_tick, Some(99));
	assert_eq!(a.skills, skills, "XP survives");
	// The stored arrangement lands on the SAME slots in the wider grid,
	// with the new rows empty — a migration, not a reshuffle.
	let l = a.layout.clone().expect("layout survives");
	assert_eq!(l.hotbar, layout().hotbar);
	assert_eq!(l.grid[17], BlockType::Stone as u16);
	assert!(
		l.grid[narrow..].iter().all(|&w| w == WIRE_EMPTY),
		"the rows the old file never had come back empty"
	);
	// The record behind it parsed too — an over-read would have eaten
	// its key and lost it entirely.
	assert_eq!(s.get(&id_b).inventory.count(BlockType::Dirt), 5);

	// And the next write stores the sized form, which round-trips at
	// the full width.
	s.put(id_a, PlayerRecord { ..a });
	s.sync();
	let s = PlayerStore::open(&dir).unwrap();
	let back = s.get(&id_a).layout.expect("still there");
	assert_eq!(back.grid[17], BlockType::Stone as u16);
	assert_eq!(back.grid.len(), GRID_SLOTS);
	assert_eq!(s.get(&id_a).skills, skills);
}

/// The sized blob is read against the lengths it declares, not the
/// constants of the day, so a file from a FUTURE build (a wider grid)
/// truncates cleanly instead of desyncing the record stream.
#[test]
fn a_wider_stored_grid_truncates_without_desyncing() {
	let dir = temp_dir("widergrid");
	std::fs::create_dir_all(&dir).unwrap();
	let id_a = key(23);
	let id_b = key(24);
	let wide = GRID_SLOTS + 9;
	let mut buf = Vec::new();
	buf.extend_from_slice(&MAGIC);
	buf.extend_from_slice(&VERSION.to_le_bytes());
	buf.extend_from_slice(&[0, 0]);
	buf.extend_from_slice(&2u32.to_le_bytes());
	for (id, dirt) in [(id_a, 3u32), (id_b, 4)] {
		buf.extend_from_slice(&id.0);
		buf.push(1);
		buf.extend_from_slice(&(BlockType::Dirt as u16).to_le_bytes());
		buf.extend_from_slice(&dirt.to_le_bytes());
		buf.push(LAYOUT_SIZED);
		buf.push(HOTBAR_SLOTS as u8);
		buf.push(wide as u8);
		for w in layout().hotbar.iter() {
			buf.extend_from_slice(&w.to_le_bytes());
		}
		for i in 0..wide {
			let w = if i == 17 {
				BlockType::Stone as u16
			} else {
				WIRE_EMPTY
			};
			buf.extend_from_slice(&w.to_le_bytes());
		}
		buf.push(0); // no position
		buf.push(0); // no return tick
		buf.push(0); // no skills
		buf.push(0); // empty spell bar (v11+)
	}
	buf.extend_from_slice(&0u32.to_le_bytes()); // empty quarantine
	std::fs::write(dir.join("inventories.bin"), &buf).unwrap();

	let s = PlayerStore::open(&dir).unwrap();
	let a = s.get(&id_a).layout.expect("layout survives");
	assert_eq!(a.grid[17], BlockType::Stone as u16);
	assert_eq!(
		s.get(&id_b).inventory.count(BlockType::Dirt),
		4,
		"the extra words were consumed, not read as the next field"
	);
}

/// The spell-bar bytes round-trip verbatim — including values the client
/// side would call junk, because the store is deliberately opaque — and a
/// record that never set them reads empty.
#[test]
fn spell_bar_bytes_round_trip_opaquely() {
	let dir = temp_dir("spellbar");
	let id = key(31);
	{
		let mut s = PlayerStore::open(&dir).unwrap();
		s.put(
			id,
			PlayerRecord {
				spells: vec![100, 0, 200],
				..PlayerRecord::default()
			},
		);
		s.put(key(32), PlayerRecord::default());
		s.sync();
	}
	let s = PlayerStore::open(&dir).unwrap();
	assert_eq!(s.get(&id).spells, vec![100, 0, 200]);
	assert!(s.get(&key(32)).spells.is_empty(), "never set reads empty");
}

/// An operator moving a player's save onto the account they play under now (`--move-save`, the
/// master's ids giving way to device keys): the old character arrives where it stood, anything
/// the new account picked up is added on, and the old key is left empty so a repeat moves nothing.
#[test]
fn a_moved_save_arrives_whole_and_leaves_nothing_behind() {
	let dir = temp_dir("movesave");
	let record = |dirt: u32, pos: [f32; 3]| {
		let mut inventory = Inventory::default();
		inventory.add(BlockType::Dirt, dirt);
		PlayerRecord {
			inventory,
			pos: Some(pos),
			..PlayerRecord::default()
		}
	};
	let (old, new) = (key(1), key(2));
	{
		let mut s = PlayerStore::open(&dir).unwrap();
		s.put(old, record(7, [10.0, 20.0, 30.0]));
		s.put(new, record(2, [0.0, 64.0, 0.0]));
		s.sync();
	}
	assert!(PlayerStore::move_save(&dir, old, new).unwrap());
	let s = PlayerStore::open(&dir).unwrap();
	assert_eq!(
		s.get(&new).inventory.count(BlockType::Dirt),
		9,
		"both piles, added"
	);
	assert_eq!(
		s.get(&new).pos,
		Some([10.0, 20.0, 30.0]),
		"back where the old one stood"
	);
	assert_eq!(
		s.get(&old).inventory.count(BlockType::Dirt),
		0,
		"moved, not copied"
	);
	drop(s);
	assert!(
		!PlayerStore::move_save(&dir, old, new).unwrap(),
		"nothing left to move"
	);
}
