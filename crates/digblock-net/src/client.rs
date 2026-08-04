//! `NetClient`: owns the transport plus the prediction/edit/interp
//! modules, decodes wire messages, and hands the engine plain events —
//! the engine never touches wire bytes.

use crate::codec::{self, FrameReader};
use crate::edits::{PendingEdit, PendingEdits};
use crate::messages::{ClientMsg, Datagram, EditKind, ServerMsg};
use crate::predict::Predictor;
use crate::protocol::{
	decode_preamble, encode_preamble, sanitize_name, PlayerId, CLIENT_TICK_LEAD,
	CONNECT_TIMEOUT_SECS, HANDSHAKE_TIMEOUT_SECS, INPUT_SEND_STRIDE, MIN_PROTOCOL, PREAMBLE_LEN,
	PROTOCOL_VERSION,
};
use crate::remote::{RemotePlayers, RemoteSnap};
use crate::transport::{ConnStatus, NetTransport, TransportEvent};
use digblock_core::block::BlockType;
use glam::Vec3;

pub struct NetClientConfig {
	pub name: String,
	pub draw_dist: i32,
	/// One-time join token minted by the master server; sent in `Hello`
	/// and verified server-side before admission. All zeros = no token
	/// (loopback tests only — real servers refuse it).
	pub token: [u8; 16],
	/// Persistent client identity secret; keys the server-side persisted
	/// inventory. All zeros = anonymous (session-only inventory).
	pub identity: [u8; 16],
}

#[derive(Debug)]
pub enum NetEvent {
	/// Handshake complete: rebuild the world around the server's seed.
	Welcome {
		seed: f64,
		player_id: PlayerId,
		spawn: Vec3,
		server_tick: u32,
		/// The server's world clock — anchor the sun here.
		tod: f32,
	},
	Refused {
		reason: String,
	},
	/// Local-player slice of a snapshot (remote players are consumed
	/// internally by `remotes`).
	Ack {
		acked_input_tick: u32,
		me: digblock_core::player::PlayerState,
	},
	/// Authoritative block value for a cell — apply directly (no-op if
	/// already predicted). `superseded` carries any of our own pending
	/// predictions this write cancelled: revert their `inv_delta`s only,
	/// never their cells (the authoritative value owns the cell).
	Edit {
		cell: (i32, i32, i32),
		block: BlockType,
		superseded: Vec<PendingEdit>,
		/// `Some(author)` only for a live broadcast authored by someone
		/// other than us (SERVER_ID = the fluid sim). `None` for join
		/// sync and for the echo of our own edits — the audio layer keys
		/// off this so a join replay never plays a thud storm.
		remote_author: Option<PlayerId>,
	},
	/// Our predicted edit was rejected and no authoritative write has
	/// superseded it — revert the cell to `.prev` AND the predicted
	/// `.inv_delta`.
	EditRejected(PendingEdit),
	/// Authoritative inventory snapshot: re-base the local counts as
	/// `snapshot + Σ pending inv_deltas` (order-independent with the
	/// per-edit reverts).
	Inventory {
		counts: Vec<(BlockType, u32)>,
	},
	/// Another player joined. Also delivered as a roster right after
	/// Welcome: one event per player already online.
	PlayerJoined {
		id: PlayerId,
		name: String,
	},
	PlayerLeft {
		id: PlayerId,
	},
	/// A chat line, already server-sanitized. `from` may be our own id
	/// (the echo confirms delivery).
	Chat {
		from: PlayerId,
		text: String,
	},
	/// The server teleported us (return to spawn). Hard-set the player
	/// there — the predictor has already dropped its pre-teleport buffer
	/// and in-flight acks.
	Teleported {
		pos: Vec3,
	},
	/// World-clock heartbeat: hard-set the sun's angle (inter-beat drift
	/// is far below visible, so no smoothing is needed).
	TimeOfDay {
		tod: f32,
	},
	Disconnected {
		reason: String,
	},
}

pub struct NetClient {
	transport: Box<dyn NetTransport>,
	pub predictor: Predictor,
	pub edits: PendingEdits,
	pub remotes: RemotePlayers,
	my_id: Option<PlayerId>,
	frames: FrameReader,
	config: NetClientConfig,
	scratch: Vec<TransportEvent>,
	/// Server preamble bytes collected so far (the first PREAMBLE_LEN
	/// reliable bytes are the preamble, not frame data).
	pre_buf: Vec<u8>,
	server_protocol: Option<u16>,
	session_token: Option<[u8; 16]>,
	/// Time spent in `Connecting` / connected-but-no-Welcome, for the
	/// handshake deadlines (sans-IO: time arrives via `poll(dt)`).
	connect_elapsed: f32,
	handshake_elapsed: f32,
	/// Refused or timed out: stop emitting further failures.
	failed: bool,
	/// Predictor tick of the last input datagram (send-rate throttle).
	last_input_send_tick: u32,
}

impl NetClient {
	pub fn new(transport: Box<dyn NetTransport>, mut config: NetClientConfig) -> Self {
		config.name = sanitize_name(&config.name);
		Self {
			transport,
			predictor: Predictor::new(0),
			edits: PendingEdits::default(),
			remotes: RemotePlayers::default(),
			my_id: None,
			frames: FrameReader::default(),
			config,
			scratch: Vec::new(),
			pre_buf: Vec::with_capacity(PREAMBLE_LEN),
			server_protocol: None,
			session_token: None,
			connect_elapsed: 0.0,
			handshake_elapsed: 0.0,
			failed: false,
			last_input_send_tick: 0,
		}
	}

	/// Handshake finished (Welcome received)?
	pub fn ready(&self) -> bool {
		self.my_id.is_some()
	}

	/// Token issued by the server in `Welcome`, for a future resume.
	pub fn session_token(&self) -> Option<[u8; 16]> {
		self.session_token
	}

	pub fn status(&self) -> ConnStatus {
		self.transport.status()
	}

	/// Drain the transport and demux into engine-shaped events. `dt` is
	/// the frame delta; it drives the connect/handshake deadlines.
	pub fn poll(&mut self, out: &mut Vec<NetEvent>, dt: f32) {
		self.check_deadlines(out, dt);
		let mut scratch = std::mem::take(&mut self.scratch);
		scratch.clear();
		self.transport.poll(&mut scratch);
		for ev in scratch.drain(..) {
			match ev {
				TransportEvent::Connected => self.send_hello(),
				TransportEvent::Reliable(bytes) => {
					let Some(bytes) = self.consume_preamble(bytes, out) else {
						continue;
					};
					self.frames.extend(&bytes);
					loop {
						match self.frames.next_frame() {
							Ok(Some(frame)) => match codec::decode::<ServerMsg>(&frame) {
								Ok(msg) => self.on_server_msg(msg, out),
								Err(e) => log::warn!("bad server frame: {e}"),
							},
							Ok(None) => break,
							Err(e) => {
								log::warn!("reliable stream corrupt: {e}");
								out.push(NetEvent::Disconnected {
									reason: format!("stream corrupt: {e}"),
								});
								break;
							}
						}
					}
				}
				TransportEvent::Datagram(bytes) => match codec::decode_datagram(&bytes) {
					Ok(Datagram::Snapshot(snap)) => self.on_snapshot(snap, out),
					Ok(Datagram::Input(_)) => {} // server-bound; ignore
					Err(e) => log::debug!("bad datagram: {e}"),
				},
				TransportEvent::Disconnected { reason } => {
					out.push(NetEvent::Disconnected { reason });
				}
			}
		}
		self.scratch = scratch;
	}

	fn check_deadlines(&mut self, out: &mut Vec<NetEvent>, dt: f32) {
		if self.failed || self.ready() {
			return;
		}
		match self.transport.status() {
			ConnStatus::Connecting => {
				self.connect_elapsed += dt;
				if self.connect_elapsed > CONNECT_TIMEOUT_SECS {
					self.failed = true;
					out.push(NetEvent::Disconnected {
						reason: "connect timeout".into(),
					});
				}
			}
			ConnStatus::Connected => {
				self.handshake_elapsed += dt;
				if self.handshake_elapsed > HANDSHAKE_TIMEOUT_SECS {
					self.failed = true;
					out.push(NetEvent::Disconnected {
						reason: "handshake timeout (incompatible server?)".into(),
					});
				}
			}
			ConnStatus::Closed => {}
		}
	}

	/// The first PREAMBLE_LEN reliable bytes are the server's preamble.
	/// Returns the remaining frame bytes once it has been consumed and
	/// validated, or None while still collecting / after failure.
	fn consume_preamble(&mut self, bytes: Vec<u8>, out: &mut Vec<NetEvent>) -> Option<Vec<u8>> {
		if self.failed {
			return None;
		}
		if self.server_protocol.is_some() {
			return Some(bytes);
		}
		let need = PREAMBLE_LEN - self.pre_buf.len();
		let take = need.min(bytes.len());
		self.pre_buf.extend_from_slice(&bytes[..take]);
		if self.pre_buf.len() < PREAMBLE_LEN {
			return None;
		}
		let pre: [u8; PREAMBLE_LEN] = self.pre_buf[..].try_into().expect("preamble len");
		let Some(version) = decode_preamble(&pre) else {
			self.failed = true;
			out.push(NetEvent::Disconnected {
				reason: "not a DigBlock server (bad preamble)".into(),
			});
			return None;
		};
		self.server_protocol = Some(version);
		if !(MIN_PROTOCOL..=PROTOCOL_VERSION).contains(&version) {
			// Newer servers may still Refuse us politely after this; going
			// through Refused locally keeps one code path either way.
			self.failed = true;
			out.push(NetEvent::Refused {
				reason: format!(
					"server protocol {version}, client supports {MIN_PROTOCOL}..={PROTOCOL_VERSION}"
				),
			});
			return None;
		}
		Some(bytes[take..].to_vec())
	}

	fn send_hello(&mut self) {
		let mut buf = encode_preamble(PROTOCOL_VERSION).to_vec();
		let hello = ClientMsg::Hello {
			protocol: PROTOCOL_VERSION,
			name: self.config.name.clone(),
			draw_dist: self.config.draw_dist,
			token: self.config.token,
			sim_version: digblock_core::SIM_VERSION,
			build: env!("CARGO_PKG_VERSION").into(),
			identity: self.config.identity,
		};
		if codec::write_frame(&hello, &mut buf).is_ok() {
			self.transport.send_reliable(&buf);
		}
	}

	fn on_server_msg(&mut self, msg: ServerMsg, out: &mut Vec<NetEvent>) {
		if self.failed {
			return; // refused/timed out: nothing after that counts
		}
		match msg {
			ServerMsg::Refused { reason } => {
				self.failed = true;
				out.push(NetEvent::Refused { reason });
			}
			ServerMsg::Welcome {
				protocol: _, // gated by the preamble before any decode
				seed,
				player_id,
				spawn,
				server_tick,
				snapshot_divisor: _,
				session_token,
				sim_version,
				build,
				tod,
			} => {
				if sim_version != digblock_core::SIM_VERSION {
					self.failed = true;
					out.push(NetEvent::Refused {
						reason: format!(
							"server sim {sim_version} (build {build}), client sim {} — update the older side",
							digblock_core::SIM_VERSION
						),
					});
					return;
				}
				self.my_id = Some(player_id);
				self.session_token = Some(session_token);
				self.predictor = Predictor::new(server_tick + CLIENT_TICK_LEAD);
				self.last_input_send_tick = 0;
				out.push(NetEvent::Welcome {
					seed,
					player_id,
					spawn: Vec3::from_array(spawn),
					server_tick,
					tod,
				});
			}
			ServerMsg::JournalColumn { cells, .. } => {
				for c in cells {
					if let Some(block) = block_from_id(c.block) {
						out.push(NetEvent::Edit {
							cell: (c.x, c.y, c.z),
							block,
							superseded: Vec::new(),
							remote_author: None,
						});
					}
				}
			}
			ServerMsg::JournalDone { .. } => {}
			ServerMsg::EditApplied {
				by,
				client_seq,
				edit,
				..
			} => {
				if Some(by) == self.my_id {
					self.edits.confirm(client_seq);
				}
				let cell = (edit.x, edit.y, edit.z);
				let superseded = self.edits.on_authoritative(cell);
				if let Some(block) = block_from_id(edit.block) {
					out.push(NetEvent::Edit {
						cell,
						block,
						superseded,
						remote_author: (Some(by) != self.my_id).then_some(by),
					});
				} else {
					// Unknown block id from a newer server: the cell can't
					// be applied, but the inventory reverts still must be.
					for e in superseded {
						out.push(NetEvent::EditRejected(e));
					}
				}
			}
			ServerMsg::EditRejected { client_seq, reason } => {
				log::info!("edit {client_seq} rejected: {reason:?}");
				if let Some(e) = self.edits.reject(client_seq) {
					out.push(NetEvent::EditRejected(e));
				}
			}
			// Remote positions come from snapshots; these only carry names.
			ServerMsg::PlayerJoined { id, name } => out.push(NetEvent::PlayerJoined { id, name }),
			ServerMsg::PlayerLeft { id } => {
				self.remotes.remove(id);
				out.push(NetEvent::PlayerLeft { id });
			}
			ServerMsg::Chat { from, text } => out.push(NetEvent::Chat { from, text }),
			ServerMsg::Teleported { pos } => {
				self.predictor.on_teleport();
				out.push(NetEvent::Teleported {
					pos: Vec3::from_array(pos),
				});
			}
			ServerMsg::TimeOfDay { tod } => out.push(NetEvent::TimeOfDay { tod }),
			ServerMsg::InventoryFull { counts } => {
				// Strict decode; unknown ids can't occur with a matching
				// sim_version but must not panic either.
				let counts = counts
					.into_iter()
					.filter_map(|(id, n)| block_from_id(id).map(|b| (b, n)))
					.collect();
				out.push(NetEvent::Inventory { counts });
			}
		}
	}

	fn on_snapshot(&mut self, snap: crate::messages::Snapshot, out: &mut Vec<NetEvent>) {
		for p in &snap.players {
			if Some(p.id) == self.my_id {
				out.push(NetEvent::Ack {
					acked_input_tick: snap.your_last_input_tick,
					me: digblock_core::player::PlayerState {
						pos: Vec3::from_array(p.pos),
						vel: Vec3::from_array(p.vel),
					},
				});
			} else {
				self.remotes.push(
					p.id,
					RemoteSnap {
						server_tick: snap.server_tick,
						pos: Vec3::from_array(p.pos),
						vel: Vec3::from_array(p.vel),
						look: Vec3::from_array(p.look),
					},
				);
			}
		}
	}

	/// One datagram carrying the current input redundancy window. Called
	/// every frame; actually sends at most once per INPUT_SEND_STRIDE
	/// predicted ticks — a 120 Hz display no longer transmits 120
	/// datagrams/s, half of them carrying an unchanged window.
	pub fn send_inputs(&mut self) {
		if !self.ready() {
			return;
		}
		let cur = self.predictor.current_tick();
		if cur.wrapping_sub(self.last_input_send_tick) < INPUT_SEND_STRIDE {
			return;
		}
		let batch = self.predictor.input_batch();
		if batch.inputs.is_empty() {
			return;
		}
		match codec::encode_datagram(&Datagram::Input(batch)) {
			Ok(bytes) => {
				self.transport.send_datagram(&bytes);
				self.last_input_send_tick = cur;
			}
			Err(e) => log::warn!("input datagram: {e}"),
		}
	}

	#[allow(clippy::too_many_arguments)]
	pub fn send_edit(
		&mut self,
		seq: u32,
		tick: u32,
		origin: Vec3,
		dir: Vec3,
		kind: EditKind,
		cell: (i32, i32, i32),
		normal: (i32, i32, i32),
	) {
		let msg = ClientMsg::EditRequest {
			seq,
			tick,
			origin: origin.to_array(),
			dir: dir.to_array(),
			kind,
			cell: [cell.0, cell.1, cell.2],
			normal: [normal.0 as i8, normal.1 as i8, normal.2 as i8],
		};
		let mut buf = Vec::new();
		if codec::write_frame(&msg, &mut buf).is_ok() {
			self.transport.send_reliable(&buf);
		}
	}

	/// Announce a hold-to-dig beginning on `cell` (reliable, in-order
	/// with the completing `send_edit` — the server times the interval).
	pub fn send_dig_start(&mut self, cell: (i32, i32, i32)) {
		self.send_msg(&ClientMsg::DigStart {
			cell: [cell.0, cell.1, cell.2],
		});
	}

	/// The hold was released or retargeted before completing.
	pub fn send_dig_cancel(&mut self) {
		self.send_msg(&ClientMsg::DigCancel);
	}

	/// Ask the server to teleport us back to spawn. The engine gates on
	/// the shared cooldown; the server enforces it authoritatively.
	pub fn send_return_to_spawn(&mut self) {
		self.send_msg(&ClientMsg::ReturnToSpawn);
	}

	/// Send a chat line. The server sanitizes and rate-limits; the local
	/// truncation only keeps a pathological buffer off the wire.
	pub fn send_chat(&mut self, text: &str) {
		let text: String = text.chars().take(crate::protocol::MAX_CHAT_LEN).collect();
		if !text.trim().is_empty() {
			self.send_msg(&ClientMsg::Chat { text });
		}
	}

	fn send_msg(&mut self, msg: &ClientMsg) {
		let mut buf = Vec::new();
		if codec::write_frame(msg, &mut buf).is_ok() {
			self.transport.send_reliable(&buf);
		}
	}
}

fn block_from_id(v: u16) -> Option<BlockType> {
	BlockType::try_from_id(v)
}
