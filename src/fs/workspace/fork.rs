//! Forking an open workspace (`gitfold branch --with-changes` while mounted):
//! its state copied under its lock. The rest of a fork, mounted or not, is
//! `state::fork`.

use std::path::Path;

use anyhow::Result;

use super::Workspace;
use crate::state::fork::copy_state_dir;

impl Workspace {
    /// Copies this open workspace's state into `dst` (see `copy_state_dir`),
    /// with every write through the mount held off meanwhile.
    pub fn copy_state(&self, dst: &Path) -> Result<()> {
        let mut layers = self.layers.write();
        // Copy-ups write under a shared `layers`; none is half-done now.
        layers.times.save()?;
        layers.markers.sync()?;
        copy_state_dir(&self.common_dir, &self.branch, &self.state_dir, dst)
    }
}
