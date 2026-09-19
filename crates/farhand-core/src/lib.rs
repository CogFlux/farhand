//! FarHand core: a coding agent's hands on a remote machine, with the local
//! machine kept closed except for what the user explicitly opens.
//!
//! - [`config`] — where the remote is and what the local side may touch.
//! - [`guard`] — the secret guard every outbound byte passes through.
//! - [`local`] — the local allowlist.
//! - [`remote`] — the SSH session and the remote operations.
//! - [`transfer`] — uploads and downloads across the two.
//! - [`audit`] — the JSONL record of everything that happened.

pub mod audit;
pub mod config;
pub mod error;
pub mod guard;
pub mod local;
pub mod remote;
pub mod text;
pub mod transfer;

pub use config::Config;
pub use error::{Error, Result};
