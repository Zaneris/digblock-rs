//! Multiplayer protocol and client netcode, sans-IO. This crate never
//! touches a socket or an async runtime: bytes go in and out through the
//! [`transport::NetTransport`] trait, so the same code drives the native
//! wtransport client (digblock-net-native), the in-memory loopback used
//! by tests, and a future wasm WebTransport shim.

pub mod client;
pub mod codec;
pub mod edits;
pub mod loopback;
pub mod messages;
pub mod predict;
pub mod protocol;
pub mod remote;
pub mod transport;

pub use client::{NetClient, NetEvent};
pub use protocol::PlayerId;
