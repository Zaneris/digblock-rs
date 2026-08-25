//! The skill outbox: which accounts' XP totals have moved since the master last heard them.
//! Filled on the sim thread under [`crate::player_store::PlayerStore::put`] — the one write every
//! change of skills already goes through — and drained by the heartbeat task, which posts a
//! batch to a master that trusts this server and discards it otherwise. Totals, never deltas:
//! a row is idempotent, so a failed post is simply put back and a newer push supersedes it.

use digblock_core::skills::SkillSet;
use digblock_net::protocol::AccountId;
use rustc_hash::FxHashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct SkillOutbox {
	pending: Mutex<FxHashMap<AccountId, SkillSet>>,
}

impl SkillOutbox {
	pub fn new() -> Self {
		Self::default()
	}

	/// Queue an account's current totals; a later push for the same
	/// account replaces an earlier one still waiting.
	pub fn push(&self, account: AccountId, skills: &SkillSet) {
		self.pending
			.lock()
			.expect("outbox mutex")
			.insert(account, skills.clone());
	}

	/// Take up to `max` rows for one report. Which rows go first is
	/// unspecified; every row goes eventually.
	pub fn drain(&self, max: usize) -> Vec<(AccountId, SkillSet)> {
		let mut pending = self.pending.lock().expect("outbox mutex");
		let keys: Vec<AccountId> = pending.keys().take(max).copied().collect();
		keys.into_iter()
			.filter_map(|k| pending.remove(&k).map(|s| (k, s)))
			.collect()
	}

	/// Put a failed report's rows back — UNDER anything pushed since, which
	/// is newer and carries at least the same totals.
	pub fn restore(&self, rows: Vec<(AccountId, SkillSet)>) {
		let mut pending = self.pending.lock().expect("outbox mutex");
		for (k, s) in rows {
			pending.entry(k).or_insert(s);
		}
	}

	pub fn len(&self) -> usize {
		self.pending.lock().expect("outbox mutex").len()
	}

	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use digblock_core::skills::Skill;

	fn set(mining: u64) -> SkillSet {
		let mut s = SkillSet::default();
		s.add_xp(Skill::Mining, mining);
		s
	}

	#[test]
	fn a_later_push_replaces_the_one_still_waiting() {
		let out = SkillOutbox::new();
		let id = AccountId([1; 16]);
		out.push(id, &set(10));
		out.push(id, &set(20));
		assert_eq!(out.drain(usize::MAX), vec![(id, set(20))]);
		assert!(out.is_empty());
	}

	#[test]
	fn a_restored_row_never_covers_a_newer_push() {
		let out = SkillOutbox::new();
		let id = AccountId([2; 16]);
		out.push(id, &set(10));
		let batch = out.drain(usize::MAX);
		out.push(id, &set(30));
		out.restore(batch);
		assert_eq!(out.drain(usize::MAX), vec![(id, set(30))]);
	}

	#[test]
	fn a_drain_is_bounded_and_the_rest_waits() {
		let out = SkillOutbox::new();
		for i in 0..5u8 {
			out.push(AccountId([i; 16]), &set(1));
		}
		assert_eq!(out.drain(2).len(), 2);
		assert_eq!(out.len(), 3);
	}
}
