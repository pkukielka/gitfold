//! The control requests the daemon serves on workspaces: `gitfold branch
//! --with-changes` while mounted (see ARCHITECTURE.md, "Forks: commands on a
//! workspace's stored state") and the request lock long ones take, and
//! applying a branch's prewarm mode or read tracking (`gitfold prewarm`,
//! `gitfold track-reads`) to its open workspace.

use std::time::Duration;

use log::warn;

use super::Workspaces;
use super::busy::Mark;
use crate::prewarm::{self, Prewarm, Spawned};
use crate::state::fork;

/// How long unmounting waits for long requests in progress.
const CLOSE_WAIT: Duration = Duration::from_secs(60);

impl Workspaces {
    /// Stops taking long requests and waits for those in progress (unmount),
    /// up to `CLOSE_WAIT`: one stuck in a fetch mustn't keep the daemon alive.
    pub fn close_requests(&self) {
        match self.requests.try_write_for(CLOSE_WAIT) {
            Some(mut closed) => *closed = true,
            None => warn!(
                "a copy of a workspace still running after {} s: exiting anyway",
                CLOSE_WAIT.as_secs()
            ),
        }
    }

    /// Held while a long request works (see `requests`); refused once closed.
    fn request(&self) -> anyhow::Result<parking_lot::RwLockReadGuard<'_, bool>> {
        let open = self.requests.read();
        anyhow::ensure!(!*open, "the mount is being unmounted");
        Ok(open)
    }

    /// `gitfold branch --with-changes` while mounted: creates branch `to` at
    /// `from`'s tip with a copy of `from`'s uncommitted changes (see
    /// `state::fork`), copying an open `from` under its lock. Both branches
    /// are busy meanwhile.
    pub fn fork(&self, from: &str, to: &str) -> anyhow::Result<String> {
        anyhow::ensure!(from != to, "a branch can't be created from itself");
        let _request = self.request()?;
        // The target first: once a reader of `from`, it takes `creating` no more
        // (an open of `from` waiting for it may hold that).
        let (_, _to) = self.mark_busy(to, Mark::Exclusive)?;
        let (ws, _from) = self.mark_busy(from, Mark::Reader)?;
        let (common, mount) = (&self.common_dir, Some(self.mount_point.as_path()));
        match ws {
            Some(ws) => fork::fork(common, from, to, mount, |dst| ws.copy_state(dst)),
            None => fork::fork(common, from, to, mount, |dst| {
                fork::copy_stored(common, from, dst)
            }),
        }
    }

    /// `gitfold track-reads`: starts or stops recording reads in the workspace
    /// of `branch` as its setting (just set) says, if it is open. Says what it
    /// did.
    pub fn track_reads_now(&self, branch: &str) -> anyhow::Result<String> {
        let Some(ws) = self.get_open(branch) else {
            return Ok(format!("{branch} isn't open: applied when it opens"));
        };
        let on = self.tracks_reads(branch);
        ws.track_reads(on)?;
        // A content walk running would be recorded as the agent's reads: walked
        // again as meta (see `branch_mode`).
        let path = self.mount_point.join(branch);
        let content = self.configured_prewarm(branch) == Prewarm::Content;
        if on && content && self.prewarm != Prewarm::None && prewarm::stop_under(&path) {
            prewarm::spawn(path, Prewarm::Meta, self.prewarm_threads);
        }
        Ok(match on {
            // Files already read are cached by the kernel: their next reads
            // aren't seen.
            true => format!("recording reads in {branch} from now on"),
            false => format!("stopped recording reads in {branch}"),
        })
    }

    /// `gitfold prewarm`: walks the workspace of `branch` in its mode (just set)
    /// if it is open, from the start (a walk in another mode may be running),
    /// or stops walking it for `none`. Says what it did.
    pub fn prewarm_now(&self, branch: &str) -> String {
        let path = self.mount_point.join(branch);
        if self.get_open(branch).is_none() {
            return format!("{branch} isn't open: walked when it opens");
        }
        match self.branch_mode(branch) {
            Prewarm::None => match prewarm::stop_under(&path) {
                true => format!("stopped walking {}", path.display()),
                false => format!("{} isn't being walked", path.display()),
            },
            _ if self.prewarm == Prewarm::None => {
                "this mount walks nothing (--prewarm none): remount to walk".to_string()
            }
            mode => {
                prewarm::stop_under(&path);
                let spawned = prewarm::spawn(path.clone(), mode, self.prewarm_threads);
                let path = path.display();
                match spawned {
                    Spawned::Now => format!("walking {path} ({mode})"),
                    Spawned::Queued => format!("{path} queued for a walk ({mode})"),
                    Spawned::Paused => {
                        format!(
                            "walks are stopped for an unmount: {path} is walked ({mode}) if it fails"
                        )
                    }
                    // Started meanwhile, by another request.
                    Spawned::Known => format!("{path} is being walked"),
                }
            }
        }
    }
}
