//! The agents in a workspace (layers-design/15-tree-spec.md, section 3:
//! "Agent presence is current, not historical"), and what following told
//! them.
//!
//! - **Presence:** `agents/<id>` in its state directory, one file per Claude
//!   Code session or subagent (its id, escaped as `workspace_id` escapes a
//!   branch), holding the id. Its hooks make or refresh it (session start,
//!   each turn start and tool call) and remove it (session end, subagent
//!   stop): one per id, so a subagent stopping leaves its session's. An
//!   agent counts as present while a file is younger than `TIMEOUT`: a
//!   session that dies without ending stops counting then, and its file
//!   goes at the next refresh.
//! - **Told:** `told/<session>`, the pause of following last reported to
//!   the session in full (its one-line summary): then it
//!   gets only a reminder each turn.
//! - **Followed:** `followed`, the moves following made while no agent was
//!   there, said at its agent's next turn start.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::Result;

use super::{StateDir, workspace_id};

/// How long an agent counts as present after its last hook.
pub const TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long a mark is fresh enough not to be refreshed.
const REFRESH: Duration = Duration::from_secs(60);

impl StateDir {
    fn agents(&self) -> PathBuf {
        self.dir.join("agents")
    }

    /// Marks agent `id` present in the workspace (refreshed if it is),
    /// if the workspace exists.
    pub fn mark_agent(&self, id: &str) -> Result<()> {
        let path = self.agents().join(workspace_id(id));
        // Refreshed lately: a burst of tool calls costs a stat each.
        let age = fs::metadata(&path).and_then(|m| m.modified());
        if age.is_ok_and(|t| t.elapsed().is_ok_and(|age| age < REFRESH)) {
            return Ok(());
        }
        if !self.dir.is_dir() {
            return Ok(());
        }
        fs::create_dir_all(self.agents())?;
        fs::write(&path, id)?;
        // Rewriting the same bytes may leave the time as it was.
        crate::fsutil::set_times(&path, None, Some(SystemTime::now()))?;
        // Those of agents gone without a word (expired) go.
        for entry in fs::read_dir(self.agents())?.flatten() {
            if !fresh(&entry) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Agent `id` left the workspace.
    pub fn unmark_agent(&self, id: &str) -> Result<()> {
        // What it was told goes with it.
        remove(&self.told_file(id))?;
        remove(&self.agents().join(workspace_id(id)))
    }

    /// Whether an agent is present in the workspace now (see the module
    /// doc).
    pub fn agent_present(&self) -> bool {
        let Ok(entries) = fs::read_dir(self.agents()) else {
            return false;
        };
        entries.flatten().any(|e| fresh(&e))
    }

    fn told_file(&self, session: &str) -> PathBuf {
        self.dir.join("told").join(workspace_id(session))
    }

    /// The pause of following last reported in full to `session`.
    pub fn told(&self, session: &str) -> Option<String> {
        fs::read_to_string(self.told_file(session)).ok()
    }

    /// Records the pause reported in full to `session` (`None`: none
    /// holds).
    pub fn set_told(&self, session: &str, pause: Option<&str>) -> Result<()> {
        let path = self.told_file(session);
        match pause {
            Some(pause) => {
                fs::create_dir_all(self.dir.join("told"))?;
                Ok(crate::fsutil::write_atomic(&path, pause.as_bytes())?)
            }
            None => remove(&path),
        }
    }

    fn followed_file(&self) -> PathBuf {
        self.dir.join("followed")
    }

    /// Records a move following made while no agent was there (`said`, what
    /// `--update` says), to tell its agent at its next turn start.
    pub fn add_followed(&self, said: &str) -> Result<()> {
        if !self.dir.is_dir() {
            return Ok(());
        }
        let mut file = fs::File::options()
            .create(true)
            .append(true)
            .open(self.followed_file())?;
        std::io::Write::write_all(&mut file, said.as_bytes())?;
        Ok(())
    }

    /// Takes the moves recorded by `add_followed`, if any.
    pub fn take_followed(&self) -> Result<Option<String>> {
        let taken = self
            .dir
            .join(format!("followed.taken-{}", std::process::id()));
        match fs::rename(self.followed_file(), &taken) {
            Err(e) if crate::fsutil::not_found(&e) => return Ok(None),
            renamed => renamed?,
        }
        let said = fs::read_to_string(&taken)?;
        fs::remove_file(&taken)?;
        Ok(Some(said))
    }
}

/// Whether presence file `entry` is younger than `TIMEOUT`.
fn fresh(entry: &fs::DirEntry) -> bool {
    (entry.metadata().and_then(|m| m.modified()))
        .is_ok_and(|t| t.elapsed().unwrap_or_default() < TIMEOUT)
}

/// Removes file `path`, if it exists.
fn remove(path: &std::path::Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if !crate::fsutil::not_found(&e) => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_is_current() {
        let tmp = std::env::temp_dir().join(format!("gitfold-agents-{}", std::process::id()));
        let state = StateDir::of(&tmp, "ws");
        state.mark_agent("s").unwrap();
        assert!(!state.agent_present(), "no workspace: nothing marked");
        fs::create_dir_all(&state.dir).unwrap();
        state.mark_agent("s").unwrap();
        state.mark_agent("a/1").unwrap();
        assert!(state.agent_present());
        state.unmark_agent("s").unwrap();
        assert!(state.agent_present(), "the subagent stays");
        let old = SystemTime::now() - TIMEOUT - Duration::from_secs(1);
        let file = fs::File::options()
            .write(true)
            .open(state.agents().join("a%2F1"))
            .unwrap();
        file.set_modified(old).unwrap();
        assert!(!state.agent_present(), "timed out");
        state.mark_agent("a/1").unwrap();
        assert!(state.agent_present(), "refreshed");
        state.unmark_agent("a/1").unwrap();
        state.unmark_agent("a/1").unwrap();
        assert!(!state.agent_present());
        fs::remove_dir_all(&tmp).unwrap();
    }
}
