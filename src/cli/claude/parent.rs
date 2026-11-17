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
    let Some((repo, state)) = workspace(input) else {
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
            let common_dir = git::init(&repo)?;
            print!("{}", turn_start(&common_dir, &state, session.as_deref())?);
            Ok(())
        }
    }
}

/// The git directory and the state of the workspace the hook with `input`
/// runs in, if it runs in one. Found without opening the repository: an
/// `inbox` hook opens the one of its own directory.
fn workspace(input: &str) -> Option<(PathBuf, StateDir)> {
    let repo = git::discover(&cwd(input).ok()?).ok()?;
    let branch = crate::cli::branch_or_current(None, &repo).ok()?;
    // `<common>/worktrees/gitfold-ws-<id>` (see `branch_or_current`).
    let common_dir = repo.parent()?.parent()?;
    let state = StateDir::of(common_dir, &branch);
    Some((repo, state))
}

/// A tool call (`inbox --claude-hook post-tool-use`, with `input`): its
/// agent (a subagent's own id, else its session's) is present in the
/// workspace it runs in. Errors only logged.
pub fn tool_used(input: &str) {
    let id = field(input, "agent_id").or_else(|| field(input, "session_id"));
    let Some((_, state)) = workspace(input) else {
        return;
    };
    let marked = mark(&state, id.as_deref());
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
/// while it lasts; a parent that looks merged noted once.
fn turn_start(common_dir: &Path, state: &StateDir, session: Option<&str>) -> Result<String> {
    let branch = &state.branch;
    let session = session.unwrap_or_default();
    let followed = follow_once(common_dir, branch);
    // Moves made while no agent was here (the daemon's): taken after
    // `follow_once`, which waits for one under way.
    let mut out = match state.take_followed()? {
        Some(said) => format!(
            "gitfold: since your last turn, {branch} took its parent's new commits:\n{said}"
        ),
        None => String::new(),
    };
    out += &match followed {
        Followed::Nothing => {
            state.set_told(session, None)?;
            String::new()
        }
        Followed::Moved(said) => {
            state.set_told(session, None)?;
            format!("gitfold: {branch} took its parent's new commits:\n{said}")
        }
        // Told once (to each session).
        Followed::Noted(said) => match state.told(session).as_deref() == Some(&said) {
            true => String::new(),
            false => {
                state.set_told(session, Some(&said))?;
                format!("gitfold: {said}\n")
            }
        },
        Followed::Paused { full, short } => match state.told(session).as_deref() == Some(&short) {
            true => format!("gitfold: following still paused: {short} (see `gitfold parent`)\n"),
            false => {
                state.set_told(session, Some(&short))?;
                format!("gitfold: following paused: {full}\n")
            }
        },
    };
    Ok(out)
}
