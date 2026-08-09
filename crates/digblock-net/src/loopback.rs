//! Paired in-memory transports with deterministic datagram impairment,
//! so prediction, reconciliation, and the whole client/server sim are
//! testable in plain `cargo test` without sockets or timers.

use crate::transport::{ConnStatus, NetTransport, TransportEvent};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Direction {
	/// Datagrams ready for delivery.
	ready: VecDeque<Vec<u8>>,
	/// Delayed datagrams: (pumps remaining, payload).
	delayed: VecDeque<(u32, Vec<u8>)>,
	/// Reliable stream chunks (delivered in order, never dropped).
	stream: VecDeque<Vec<u8>>,
	/// Sender-side policy for this direction.
	drop_every: Option<u32>,
	delay_pumps: u32,
	sent: u32,
	/// Set by the writer to tear this direction down: the reader reports
	/// `Disconnected` once whatever is already queued has drained.
	closed: Option<String>,
	/// Nothing crosses this direction any more, in either sense — the
	/// peer went away without saying so.
	silent: bool,
}

pub struct LoopbackEnd {
	/// Direction this end writes into.
	tx: Arc<Mutex<Direction>>,
	/// Direction this end reads from.
	rx: Arc<Mutex<Direction>>,
	connected_reported: bool,
	closed_reported: bool,
}

pub fn loopback_pair() -> (LoopbackEnd, LoopbackEnd) {
	let a_to_b = Arc::new(Mutex::new(Direction::default()));
	let b_to_a = Arc::new(Mutex::new(Direction::default()));
	(
		LoopbackEnd {
			tx: a_to_b.clone(),
			rx: b_to_a.clone(),
			connected_reported: false,
			closed_reported: false,
		},
		LoopbackEnd {
			tx: b_to_a,
			rx: a_to_b,
			connected_reported: false,
			closed_reported: false,
		},
	)
}

impl LoopbackEnd {
	/// Impair this end's OUTGOING datagrams: drop every `n`th sent, and
	/// delay the rest by `delay_pumps` calls to the receiver's [`pump`].
	pub fn set_datagram_policy(&mut self, drop_every: Option<u32>, delay_pumps: u32) {
		let mut d = self.tx.lock().unwrap();
		d.drop_every = drop_every;
		d.delay_pumps = delay_pumps;
	}

	/// Close this end: the PEER's next `poll` drains whatever is already
	/// queued and then reports `Disconnected`.
	///
	/// Without this the loopback transport could not express a disconnect
	/// at all — `status` was hardcoded to `Connected` and `poll` never
	/// emitted the event — so no in-process test could exercise anything
	/// the client does after `Welcome` when a connection dies. That is
	/// exactly the path the liveness deadline and the native transport's
	/// dead-thread detection live on.
	pub fn close(&mut self, reason: &str) {
		self.tx.lock().unwrap().closed = Some(reason.to_string());
	}

	/// Go silent WITHOUT closing: nothing more is delivered in this
	/// direction and no disconnect is ever reported. The peer is left to
	/// notice on its own — a hung server, or a transport that lost its
	/// background thread and cannot say so.
	pub fn go_silent(&mut self) {
		self.tx.lock().unwrap().silent = true;
	}

	/// Mature this end's incoming delayed datagrams by one step.
	pub fn pump(&mut self) {
		let mut d = self.rx.lock().unwrap();
		for entry in d.delayed.iter_mut() {
			entry.0 = entry.0.saturating_sub(1);
		}
		while matches!(d.delayed.front(), Some((0, _))) {
			let (_, bytes) = d.delayed.pop_front().unwrap();
			d.ready.push_back(bytes);
		}
	}
}

impl NetTransport for LoopbackEnd {
	fn send_datagram(&mut self, bytes: &[u8]) {
		let mut d = self.tx.lock().unwrap();
		if d.silent {
			return;
		}
		d.sent += 1;
		if let Some(n) = d.drop_every {
			if d.sent % n == 0 {
				return;
			}
		}
		if d.delay_pumps > 0 {
			let pumps = d.delay_pumps;
			d.delayed.push_back((pumps, bytes.to_vec()));
		} else {
			d.ready.push_back(bytes.to_vec());
		}
	}

	fn send_reliable(&mut self, bytes: &[u8]) {
		let mut d = self.tx.lock().unwrap();
		if d.silent {
			return;
		}
		d.stream.push_back(bytes.to_vec());
	}

	fn poll(&mut self, out: &mut Vec<TransportEvent>) {
		if !self.connected_reported {
			self.connected_reported = true;
			out.push(TransportEvent::Connected);
		}
		let mut d = self.rx.lock().unwrap();
		if d.silent {
			return;
		}
		while let Some(bytes) = d.stream.pop_front() {
			out.push(TransportEvent::Reliable(bytes));
		}
		while let Some(bytes) = d.ready.pop_front() {
			out.push(TransportEvent::Datagram(bytes));
		}
		// Queued bytes first, then the close — a real transport delivers
		// what already arrived before it notices the stream ended.
		if let Some(reason) = &d.closed {
			if !self.closed_reported {
				self.closed_reported = true;
				out.push(TransportEvent::Disconnected {
					reason: reason.clone(),
				});
			}
		}
	}

	fn status(&self) -> ConnStatus {
		if self.closed_reported {
			ConnStatus::Closed
		} else {
			ConnStatus::Connected
		}
	}
}
