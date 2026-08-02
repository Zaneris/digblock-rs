//! `NetClient`: owns the transport plus the prediction/edit/interp
//! modules, decodes wire messages, and hands the engine plain events —
//! the engine never touches wire bytes.

use crate::codec::{self, FrameReader};
use crate::edits::{PendingEdit, PendingEdits};
use crate::messages::{ClientMsg, Datagram, EditKind, ServerMsg};
use crate::predict::Predictor;
use crate::protocol::{PlayerId, CLIENT_TICK_LEAD, PROTOCOL_VERSION};
use crate::remote::{RemotePlayers, RemoteSnap};
use crate::transport::{ConnStatus, NetTransport, TransportEvent};
use digblock_core::block::BlockType;
use glam::Vec3;

pub struct NetClientConfig {
	pub name: String,
	pub draw_dist: i32,
}

#[derive(Debug)]
pub enum NetEvent {
	/// Handshake complete: rebuild the world around the server's seed.
	Welcome {
		seed: f64,
		player_id: PlayerId,
		spawn: Vec3,
		server_tick: u32,
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
	/// already predicted).
	Edit {
		cell: (i32, i32, i32),
		block: BlockType,
	},
	/// Our predicted edit was rejected and no authoritative write has
	/// superseded it — revert the cell to `.prev`.
	EditRejected(PendingEdit),
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
}

impl NetClient {
	pub fn new(transport: Box<dyn NetTransport>, config: NetClientConfig) -> Self {
		Self {
			transport,
			predictor: Predictor::new(0),
			edits: PendingEdits::default(),
			remotes: RemotePlayers::default(),
			my_id: None,
			frames: FrameReader::default(),
			config,
			scratch: Vec::new(),
		}
	}

	/// Handshake finished (Welcome received)?
	pub fn ready(&self) -> bool {
		self.my_id.is_some()
	}

	pub fn my_id(&self) -> Option<PlayerId> {
		self.my_id
	}

	pub fn status(&self) -> ConnStatus {
		self.transport.status()
	}

	/// Drain the transport and demux into engine-shaped events.
	pub fn poll(&mut self, out: &mut Vec<NetEvent>) {
		let mut scratch = std::mem::take(&mut self.scratch);
		scratch.clear();
		self.transport.poll(&mut scratch);
		for ev in scratch.drain(..) {
			match ev {
				TransportEvent::Connected => self.send_hello(),
				TransportEvent::Reliable(bytes) => {
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

	fn send_hello(&mut self) {
		let mut buf = Vec::new();
		let hello = ClientMsg::Hello {
			protocol: PROTOCOL_VERSION,
			name: self.config.name.clone(),
			draw_dist: self.config.draw_dist,
		};
		if codec::write_frame(&hello, &mut buf).is_ok() {
			self.transport.send_reliable(&buf);
		}
	}

	fn on_server_msg(&mut self, msg: ServerMsg, out: &mut Vec<NetEvent>) {
		match msg {
			ServerMsg::Refused { reason } => out.push(NetEvent::Refused { reason }),
			ServerMsg::Welcome {
				protocol,
				seed,
				player_id,
				spawn,
				server_tick,
				snapshot_divisor: _,
			} => {
				if protocol != PROTOCOL_VERSION {
					out.push(NetEvent::Refused {
						reason: format!("protocol {protocol} != {PROTOCOL_VERSION}"),
					});
					return;
				}
				self.my_id = Some(player_id);
				self.predictor = Predictor::new(server_tick + CLIENT_TICK_LEAD);
				out.push(NetEvent::Welcome {
					seed,
					player_id,
					spawn: Vec3::from_array(spawn),
					server_tick,
				});
			}
			ServerMsg::JournalColumn { cells, .. } => {
				for c in cells {
					if let Some(block) = block_from_u8(c.block) {
						out.push(NetEvent::Edit {
							cell: (c.x, c.y, c.z),
							block,
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
				self.edits.on_authoritative(cell);
				if let Some(block) = block_from_u8(edit.block) {
					out.push(NetEvent::Edit { cell, block });
				}
			}
			ServerMsg::EditRejected { client_seq, reason } => {
				log::info!("edit {client_seq} rejected: {reason:?}");
				if let Some(e) = self.edits.reject(client_seq) {
					out.push(NetEvent::EditRejected(e));
				}
			}
			ServerMsg::PlayerJoined { .. } => {} // snapshots populate remotes
			ServerMsg::PlayerLeft { id } => self.remotes.remove(id),
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

	/// One datagram carrying the current input redundancy window.
	pub fn send_inputs(&mut self) {
		if !self.ready() {
			return;
		}
		let batch = self.predictor.input_batch();
		if batch.inputs.is_empty() {
			return;
		}
		match codec::encode_datagram(&Datagram::Input(batch)) {
			Ok(bytes) => self.transport.send_datagram(&bytes),
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
}

fn block_from_u8(v: u8) -> Option<BlockType> {
	(v < BlockType::COUNT as u8).then(|| BlockType::from_u8(v))
}
