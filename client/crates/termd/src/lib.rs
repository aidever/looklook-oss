//! `looklook-termd`: persistent per-user terminal multiplexer daemon replacing tmux/ttyd.

pub mod handover;
pub mod keys;
pub mod pty;
pub mod server;
pub mod session;
pub mod transport;

pub use server::{DaemonOptions, run_daemon};
pub use transport::default_socket_endpoint;
