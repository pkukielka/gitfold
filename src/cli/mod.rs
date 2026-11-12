//! The commands run from the shell, not by the daemon.
//!
//! - `manage`: `branch`, `list`, `remove`, `prune`.
//! - `mount`: `mount` in the background, `unmount`, `version`.

pub mod manage;
pub mod mount;
