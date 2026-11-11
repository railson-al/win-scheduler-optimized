//! Local IPC between the ScriptRunner daemon and its clients (CLI today, GUI
//! later). Frames are length-prefixed JSON over a named pipe (Windows) or a
//! Unix socket (development/CI). This crate depends only on `core`, so a client
//! never pulls in SQLite.

pub mod client;
pub mod codec;
#[cfg(windows)]
mod pipe_security;
pub mod protocol;
#[cfg(windows)]
mod service_check;
pub mod transport;

pub use protocol::{
    ClientFrame, ErrorBody, ErrorCode, Event, Outcome, PROTOCOL_VERSION, Request, Response, ServerFrame, StatusInfo,
};
