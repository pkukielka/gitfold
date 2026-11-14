//! `gitfold layer` and `gitfold lens`: other workspaces' work shown in a
//! workspace, read-only, over its base (see `state::lens`). The layers are
//! stored with the workspace; while mounted, the daemon shows them at once.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use gix::ObjectId;

use crate::daemon::control::{self, Request};
use crate::git;
use crate::refusal::Refusal;
use crate::state::StateDir;
use crate::state::lens::{Layer, Lens};
use crate::state::retire::check_name;
use crate::state::seal;
use crate::store::Store;

#[derive(Args)]
pub struct LayerArgs {
    #[command(subcommand)]
    pub op: LayerOp,
    /// The workspace to show them in (default: the one you are in).
    #[arg(short = 'w', long, global = true, value_name = "BRANCH")]
    pub workspace: Option<String>,
}

#[derive(Subcommand)]
pub enum LayerOp {
    /// Show <BRANCH>'s work (uncommitted included, as now) on top of the others.
    Add { branch: String },
    /// Show a layer again.
    On { branch: String },
    /// Hide a layer (it stays in the stack).
    Off { branch: String },
    /// Drop a layer from the stack.
    Rm { branch: String },
    /// Move every layer to its workspace's work as now.
    Update,
    /// List the layers, bottom first: shown or not, and their seal.
    List,
}

#[derive(Args)]
pub struct LensArgs {
    /// `off`: hide every layer (git works again).
    #[arg(value_parser = ["off"])]
    pub op: String,
    /// The workspace (default: the one you are in).
    pub workspace: Option<String>,
}

enum Kind {
    On,
    Off,
    Rm,
}

/// `gitfold layer`.
pub fn layer(repo: &Path, args: LayerArgs) -> Result<()> {
    let name = super::branch_or_current(args.workspace, repo)?;
    check_name(&name)?;
    let common_dir = git::init(repo)?;
    let state = StateDir::of(&common_dir, &name);
    if !state.opened() {
        git::require_branch(&name)?;
        return Err(Refusal::NotFound.error(format!("{name} has no workspace yet")));
    }
    let store = Store::default();
    let mut lens = Lens::read(&state)?;
    let op_kind = match &args.op {
        LayerOp::On { .. } => Kind::On,
        LayerOp::Off { .. } => Kind::Off,
        _ => Kind::Rm,
    };
    match args.op {
        LayerOp::List => {
            print!("{}", listing(&lens));
            return Ok(());
        }
        LayerOp::Add { branch } => {
            anyhow::ensure!(branch != name, "a workspace can't be its own layer");
            if lens.find(&branch).is_some() {
                bail!("{branch} is a layer already (`gitfold layer on` shows it)");
            }
            let layer = layer_of(&store, &common_dir, &state, &branch)?;
            lens.layers.push(layer);
        }
        LayerOp::On { branch } | LayerOp::Off { branch } | LayerOp::Rm { branch } => {
            let i = lens.find(&branch).ok_or_else(|| {
                Refusal::NotFound.error(format!("{branch} isn't a layer of {name}"))
            })?;
            match op_kind {
                Kind::On => lens.layers[i].on = true,
                Kind::Off => lens.layers[i].on = false,
                Kind::Rm => {
                    lens.layers.remove(i);
                }
            }
        }
        LayerOp::Update => {
            for layer in &mut lens.layers {
                let on = layer.on;
                *layer = layer_of(&store, &common_dir, &state, &layer.branch)?;
                layer.on = on;
            }
        }
    }
    lens.write(&state)?;
    println!("{}", apply(&common_dir, &name)?);
    Ok(())
}

/// `gitfold lens off`.
pub fn lens(repo: &Path, args: LensArgs) -> Result<()> {
    let name = super::branch_or_current(args.workspace, repo)?;
    check_name(&name)?;
    let common_dir = git::init(repo)?;
    let state = StateDir::of(&common_dir, &name);
    let mut lens = Lens::read(&state)?;
    for layer in &mut lens.layers {
        layer.on = false;
    }
    lens.write(&state)?;
    println!("{}", apply(&common_dir, &name)?);
    Ok(())
}

/// The layer of `branch`'s work as now, for the workspace in `state`: its
/// seal (its tip if it has no workspace), and where the two diverged.
fn layer_of(store: &Store, common_dir: &Path, state: &StateDir, branch: &str) -> Result<Layer> {
    check_name(branch)?;
    let theirs = StateDir::of(common_dir, branch);
    let seal = match theirs.opened() {
        true => seal::seal(store, &theirs)?.commit,
        false => git::branch_tip(branch)?.id,
    };
    let base = state.read_base()?.id;
    let out = git::cmd::output(
        common_dir,
        &["merge-base", &base.to_string(), &seal.to_string()],
    )?;
    let merge_base = ObjectId::from_hex(String::from_utf8_lossy(&out).trim().as_bytes())?;
    Ok(Layer {
        branch: branch.to_string(),
        on: true,
        seal,
        merge_base,
    })
}

/// Has the daemon show the stored layers now, if mounted; says what happened.
fn apply(common_dir: &Path, name: &str) -> Result<String> {
    let request = Request::Lens {
        branch: name.to_string(),
    };
    match control::request(common_dir, &request, None)? {
        Some(done) => Ok(done),
        None => Ok(format!(
            "{name}: not mounted; its layers show when it opens"
        )),
    }
}

fn listing(lens: &Lens) -> String {
    let mut out = String::new();
    for l in &lens.layers {
        let on = if l.on { "on " } else { "off" };
        let _ = writeln!(out, "{on}\t{}\t{}", l.branch, &l.seal.to_string()[..12]);
    }
    out
}
