//! Following at an agent's safe moments, and its presence
//! (layers-design/15-tree-spec.md, section 3): `gitfold parent --claude-hook
//! EVENT`. Session start and each turn start mark the agent present in its
//! workspace, session end and a subagent's stop unmark it (see
//! `state::agents`); at turn start its branch takes its parent's new
//! commits, if it follows it, and what happened becomes the agent's context.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::cwd;
use crate::cli::hook::field;
use crate::cli::tree::{Followed, follow_once};
use crate::git;
use crate::state::StateDir;

/// The Claude Code hooks `gitfold parent` serves.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ParentHook {
    SessionStart,
    SessionEnd,
    SubagentStop,
    UserPromptSubmit,
}

/// `gitfold parent --claude-hook EVENT`: nothing outside a workspace; errors
/// only logged (the hook runs in every session).
pub fn parent_hook(event: ParentHook) -> Result<()> {
    let input = std::io::read_to_string(std::io::stdin())?;
    if let Err(e) = run(event, &input) {
        log::warn!("gitfold parent --claude-hook: {e:#}");
    }
    Ok(())
}

fn run(event: ParentHook, input: &str) -> Result<()> {
    let Some((common_dir, state)) = workspace(input)? else {
        return Ok(());
    };
    let session = field(input, "session_id");
    match event {
        ParentHook::SessionStart => mark(&state, session.as_deref()),
        ParentHook::SessionEnd => unmark(&state, session.as_deref()),
        // Its session's own presence stays.
        ParentHook::SubagentStop => unmark(&state, field(input, "agent_id").as_deref()),
        ParentHook::UserPromptSubmit => {
            mark(&state, session.as_deref())?;
            print!("{}", turn_start(&common_dir, &state, session.as_deref())?);
            Ok(())
        }
    }
}

/// The repository and the state of the workspace the hook with `input`
/// runs in, if it runs in one.
fn workspace(input: &str) -> Result<Option<(PathBuf, StateDir)>> {
    let Ok(repo) = git::discover(&cwd(input)?) else {
        return Ok(None);
    };
    let Ok(branch) = crate::cli::branch_or_current(None, &repo) else {
        return Ok(None);
    };
    let common_dir = git::init(&repo)?;
    let state = StateDir::of(&common_dir, &branch);
    Ok(Some((common_dir, state)))
}

/// A tool call (`inbox --claude-hook post-tool-use`, with `input`): its
/// agent (a subagent's own id, else its session's) is present in the
/// workspace it runs in. Errors only logged.
pub fn tool_used(input: &str) {
    let id = field(input, "agent_id").or_else(|| field(input, "session_id"));
    let marked = workspace(input).and_then(|ws| match ws {
        Some((_, state)) => mark(&state, id.as_deref()),
        None => Ok(()),
    });
    if let Err(e) = marked {
        log::warn!("gitfold inbox --claude-hook: marking the agent present: {e:#}");
    }
}

/// Marks agent `id` present in the workspace of `state` (see
/// `state::agents`).
fn mark(state: &StateDir, id: Option<&str>) -> Result<()> {
    id.map_or(Ok(()), |id| state.mark_agent(id))
}

fn unmark(state: &StateDir, id: Option<&str>) -> Result<()> {
    id.map_or(Ok(()), |id| state.unmark_agent(id))
}

/// What the agent of the workspace of `state` is told at its turn start,
/// its branch having followed its parent (`follow_once`): a move in full, a
/// pause in full once (to each session), then a one-line reminder each turn
/// while it lasts.
fn turn_start(common_dir: &Path, state: &StateDir, session: Option<&str>) -> Result<String> {
    let branch = &state.branch;
    // Moves made while no agent was here (the daemon's).
    let mut out = match state.take_followed()? {
        Some(said) => format!(
            "gitfold: since your last turn, {branch} took its parent's new commits:\n{said}"
        ),
        None => String::new(),
    };
    out += &match follow_once(common_dir, branch) {
        Followed::Nothing => {
            state.set_told(None)?;
            String::new()
        }
        Followed::Moved(said) => {
            state.set_told(None)?;
            format!("gitfold: {branch} took its parent's new commits:\n{said}")
        }
        Followed::Paused { full, short } => {
            // Told per session: a new one hears it in full.
            let told = format!("{}\n{short}", session.unwrap_or_default());
            match state.told().as_deref() == Some(&told) {
                true => {
                    format!("gitfold: following still paused: {short} (see `gitfold parent`)\n")
                }
                false => {
                    state.set_told(Some(&told))?;
                    format!("gitfold: following paused: {full}\n")
                }
            }
        }
    };
    Ok(out)
}
