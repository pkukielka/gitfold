mod bench;
mod cli;
mod daemon;
mod fs;
mod fsutil;
mod git;
mod inode;
mod inotify;
mod lock;
mod mount_table;
mod prewarm;
mod procs;
mod refusal;
mod settings;
mod shell;
mod state;
mod store;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use cli::layer::LayerArgs;

#[derive(Parser)]
#[command(about = "Git revisions as a filesystem", version = LONG_VERSION)]
struct Cli {
    /// Run as if started in PATH (like `git -C`); default: the current directory.
    #[arg(short = 'C', global = true, value_name = "PATH")]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a repository: its revisions and branches as folders.
    ///
    /// Revisions appear as <MOUNTPOINT>/@<REV>/..., branches as
    /// <MOUNTPOINT>/<BRANCH>/... Runs in the background until `gitfold
    /// unmount`.
    Mount(cli::mount::MountArgs),
    /// Unmount, and wait for gitfold to exit.
    ///
    /// Works even while a shell is inside the mount: then it detaches the mount
    /// and waits for the processes using it to leave.
    Unmount { mountpoint: PathBuf },
    /// Print this gitfold's version, and its daemon's.
    ///
    /// Given a mount point, also the version its daemon runs. Exit codes: 0 the
    /// same build, 3 another build (remount to upgrade), 4 a daemon too old to
    /// tell, 5 no answer (busy), 1 other errors.
    Version { mountpoint: Option<PathBuf> },
    /// Create a branch to open as a workspace, with a workspace's changes if asked.
    ///
    /// The workspace is <MOUNTPOINT>/<BRANCH>. With --with-changes, the branch
    /// starts at the tip of the workspace --from names (default: the one you are
    /// in), and its workspace with a copy of that one's uncommitted changes and
    /// staged state: the same files and the same `git status`, from then on each
    /// on its own. Only the changes are copied, so it costs what they weigh, not
    /// the tree. Exit codes: 3 no such branch, 4 a git operation in progress in
    /// the source, 5 the source or the new branch busy (being removed, or the
    /// new branch being made), 1 other errors, 2 usage errors.
    Branch(cli::branch::BranchArgs),
    /// List the workspaces and their changes.
    ///
    /// For each: how many files it changed and deleted (uncommitted), how many
    /// ignored files it holds, the commit it is on, how long ago it last
    /// changed (a file written, created or deleted, or a commit), and notes
    /// (branch deleted, commit behind; for a branch built on editable layers:
    /// one landed, needs update, blocked, the force-push to run), and what it
    /// builds on (its editable layers, `-` for none: main). A branch built on
    /// another is listed under it, indented with └─, as are such branches
    /// without a workspace and the layers they build on (`-` for the counts).
    List(cli::list::ListArgs),
    /// List a workspace's uncommitted changes (A/M/D per path), without git status.
    ///
    /// Against its base, untracked files included and ignored ones not, whatever
    /// is staged (as `git add -A` and `git diff --cached --name-status`): one
    /// path per line, `A` added, `M` modified, `D` deleted, quoted as git quotes
    /// paths. Read from the workspace's stored state, mounted or not.
    Changes(cli::list::ChangesArgs),
    /// List what a workspace read or changed that changed elsewhere since.
    ///
    /// One line per path and target: `W` for a path the workspace changed
    /// (committed on its branch or not) that the target changed too, `R` for a
    /// file it read (recorded while tracked: `gitfold track-reads`) that the
    /// target changed, then the target and the path, quoted as git quotes
    /// paths. A target's changes are its commits since it diverged from the
    /// workspace's base, plus, for a branch with a workspace, that workspace's
    /// uncommitted changes. Paths, not lines: a `W` path often merges cleanly
    /// (`git merge-tree` tells). Read from stored state, mounted or not. Exit
    /// codes: 0 no overlaps, 6 some, 3 no such branch (or no
    /// reads recorded, with --reads), 1 other errors, 2 usage errors.
    Overlaps(cli::overlaps::OverlapsArgs),
    /// Wait for a change in a directory of a task folder, and print it.
    ///
    /// <TARGET> is <BRANCH>:<DIR>, or <DIR> of the workspace you are in (`.`
    /// for the folder itself). Waits until a file there is written, created,
    /// moved in, deleted or moved out (not below it; dot names don't count),
    /// prints `C` (changed) or `D` (deleted), a tab and the name for it and
    /// whatever else changed right after, and exits. Plain inotify on the
    /// folder's directory: mounted or not, whoever writes.
    Watch(cli::inbox::WatchArgs),
    /// Take the messages in a workspace's inbox and print them.
    ///
    /// The inbox is `inbox/` in the task folder (`gitfold.inbox` names it). Each
    /// message is taken once, whoever else reads: printed as sent (headers, an
    /// empty line, the body), an empty line between messages, and deleted.
    Inbox(cli::inbox::InboxArgs),
    /// Leave a message in a workspace's inbox.
    ///
    /// Written atomically into <BRANCH>'s task folder: `from:` and `time:`
    /// headers, `subject:` if given, an empty line, then the text (`-`: read
    /// from stdin). The branch needs no workspace yet. Exit codes: 3 no such
    /// branch, 1 other errors.
    Send(cli::inbox::SendArgs),
    /// Show workspaces' work, uncommitted included, as a read-only folder: a
    /// snapshot to build and test.
    ///
    /// A branch with a workspace contributes its work: what the workspace
    /// shows, ignored files left out (as `gitfold changes` lists them), on its
    /// base. Other branches (and all with --commits) and revisions contribute
    /// their commits. Several are merged in order onto the first (or --onto)
    /// as `git merge-tree` does. Prints the snapshot's path,
    /// <MOUNTPOINT>/@<SHA> (unmounted, the commit). No workspace, index or
    /// branch changes; the same work gives the same snapshot. Exit codes: 3 no
    /// such branch or revision, 4 conflicts (the paths listed), 1 other
    /// errors, 2 usage errors.
    Snapshot(cli::snapshot::SnapshotArgs),
    /// Show other branches' work in a workspace (layers): see-only, or
    /// editable (--edit: build on it).
    ///
    /// A layer is another branch's work shown over the workspace's own
    /// commits and under its own changes: a workspace's work (uncommitted
    /// included), or with --commits or without a workspace, its commits. It
    /// is kept up to date (at its agent's turns, and when added again)
    /// unless --frozen; a tag or a commit never moves. A see-only layer's
    /// files can't be written there, and git sees only the workspace's own
    /// changes. An editable layer (`layer add BRANCH --edit`) is one the
    /// branch builds on: its commits sit on BRANCH's commits (on a merge of
    /// them with several), follow its new ones at its agent's turns (or,
    /// without an agent, once the workspace is idle), and come off them
    /// when it is removed or made see-only (`layer add BRANCH` again); what
    /// BRANCH committed is the branch's to edit, and a file only editable
    /// layers changed is writable, git seeing only the workspace's own lines
    /// of it. Changes to their uncommitted lines are waiting edits: kept in
    /// the file and out of git until the layer commits those lines. Every
    /// branch otherwise sits on main. A layer brings only its
    /// own changes: a file it changed shows the base's version with them
    /// merged in (as merging its branch would), one layer after another,
    /// with conflict markers where they conflict with the base's or another
    /// layer's (listed). Files marked `merge=union` in .gitattributes are
    /// union instead (the layers' lines and the workspace's own, which it
    /// writes; each workspace's lines where it put them among the others'),
    /// with a filter so git sees and commits only the workspace's own
    /// lines, and a guard refusing commits that bring the layers' onto the
    /// branch (and the daemon warning about one made without it). Where git
    /// can't be kept right, it is turned off there, saying why. With no
    /// subcommand, lists the layers
    /// (what each shows, how it updates, whether your commits sit on it),
    /// what your commits sit on, their conflicts, waiting edits, commits
    /// with a layer's uncommitted lines, notes on landings and on
    /// union files (lines placed by the base's lines, or lines added among
    /// another branch's: maybe not a list). Stored with the workspace (a
    /// branch without one: its editable layers); while mounted, shown at
    /// once. Exit codes: 3 no such workspace, branch or layer, 4 refused or
    /// not moved (git busy, commits already pushed without --yes, commits
    /// or waiting edits that change a layer's lines, a conflict, uncommitted work in the
    /// way, own lines that depend on a layer's), 1 other errors, 2 usage
    /// errors (--edit on a tag, a commit or main).
    Layer(cli::layer::LayerArgs),
    /// Propose a change to a layer's files (a contract) from a workspace
    /// showing it.
    ///
    /// Makes branch <LAYER>-prop/<WORKSPACE>, a copy of the layer's
    /// workspace with its uncommitted work (as `branch --with-changes`),
    /// where the files are yours to edit (or copied from --from), and shows
    /// it in this workspace in place of the layer. The layer's agent checks
    /// it (`gitfold consent`) and takes it (`gitfold accept`). Run again, it
    /// adds to the same proposal. Exit codes: 3 no such workspace or layer,
    /// 4 git busy, 1 other errors, 2 usage errors.
    Propose(cli::proposal::ProposeArgs),
    /// List the open proposals: who proposed, the files changed, consent.
    Proposals(cli::proposal::ProposalsArgs),
    /// Check a proposal against every workspace it affects.
    ///
    /// What accepting it gives is the layer's work as now with the
    /// proposal's changes. For each workspace showing the layer (or a
    /// proposal to change it; with --only-readers, not those whose read
    /// records say they didn't read a changed file): whether its own work
    /// merges with that, and --test run in its view with that in the
    /// layer's place (its other layers and own work included), a snapshot.
    /// No workspace changes. Prints a table: workspace, read the files (yes,
    /// no, unknown), merges cleanly, test. The result is recorded for
    /// `accept`. Exit codes: 0 all passed, 4 any failed or none was checked
    /// (or the layer changed the proposed files since), 3 no such proposal,
    /// 1 other errors.
    Consent(cli::proposal::ConsentArgs),
    /// Take a proposal into its layer's workspace.
    ///
    /// Run in the layer's workspace (-w only from outside every workspace).
    /// Needs a passed `gitfold consent` of the proposal and the layer as
    /// they are now (or --force). Writes the files it changed into the
    /// layer's workspace (uncommitted work there, as any edit), shows the
    /// layer again where the proposal showed, sends a message (`gitfold
    /// send`) to every workspace showing the layer but those whose read
    /// records say they didn't read the files, and removes the proposal's
    /// workspace and branch. Workspaces showing the layer see the change at
    /// their next turn. Exit codes: 4 the layer changed those files since
    /// the proposal was made, or no current passed consent, 3 no such
    /// proposal (or not this layer's), 1 other errors.
    Accept(cli::proposal::AcceptArgs),
    /// Remove a proposal without taking it (or withdraw your own).
    ///
    /// Run in its layer's workspace or its proposer's (-w only from outside
    /// every workspace): the layer shows again where the proposal showed,
    /// the proposer is told (with --reason), and the proposal's workspace
    /// and branch are removed. Exit codes: 3 no such proposal (or not one of
    /// these workspaces'), 1 other errors.
    Reject(cli::proposal::RejectArgs),
    /// Delete a workspace, or a snapshot's registration.
    ///
    /// A workspace: its registration, stored changes and base ref (its commits
    /// stay on the branch). A snapshot (@<rev>): its registration. Works
    /// mounted too, unless it is in use. Exit codes: 3 not found, 4 uncommitted
    /// changes (or a git operation in progress), 5 in use, 1 other errors, 2
    /// usage errors.
    Remove(cli::remove::RemoveArgs),
    /// Delete the snapshot registrations.
    ///
    /// They are registered again when opened. The repository must not be
    /// mounted.
    Prune,
    /// Set a workspace's prewarm mode.
    ///
    /// As `branch --prewarm`, kept in git config; while mounted also applied at
    /// once: the workspace is walked if open, or its walk stopped (none).
    Prewarm(cli::branch::PrewarmArgs),
    /// Record the files Claude Code reads in a workspace (its hook reports
    /// them), for `gitfold overlaps`, or stop.
    ///
    /// As `branch --track-reads`, kept in git config, whatever the mount's
    /// --track-reads says; while mounted also applied at once. Files read
    /// before tracking starts aren't recorded. Stopping keeps what was
    /// recorded.
    TrackReads(cli::branch::TrackReadsArgs),
    /// Walk a freshly opened snapshot or workspace (started by the daemon).
    #[command(hide = true)]
    PrewarmWalk {
        root: PathBuf,
        #[arg(long, value_enum)]
        mode: prewarm::Prewarm,
        #[arg(long)]
        threads: usize,
    },
    /// git's filter in a workspace with union files (its
    /// `filter.<driver>.process`): relays to the daemon.
    #[command(hide = true)]
    FilterProcess { common_dir: PathBuf, id: String },
    /// The guard, run by a workspace's hooks: checks the ref updates or
    /// pushes on stdin for layers' uncommitted lines.
    #[command(hide = true)]
    Guard {
        #[arg(value_enum)]
        hook: git::guard::Hook,
        admin: PathBuf,
    },
    /// Evaluate checkout attributes for every file of a revision.
    #[command(hide = true)]
    BenchAttrs { repo: PathBuf, rev: String },
    /// Decompress every blob of a revision and report throughput.
    #[command(hide = true)]
    BenchRead {
        repo: PathBuf,
        rev: String,
        #[arg(long, value_delimiter = ',', default_value = "1,4,12,24")]
        threads: Vec<usize>,
    },
}

/// This build: the crate version and a hash of its sources, as compared with
/// a mount's daemon (see `build.rs`).
pub const VERSION: &str = env!("GITFOLD_VERSION");
/// `VERSION` and the git commit built from, if known, for `--version`.
const LONG_VERSION: &str = env!("GITFOLD_LONG_VERSION");

fn main() -> std::process::ExitCode {
    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:?}");
            refusal::Refusal::of(&e).map_or(1, |r| r.exit_code())
        }
    };
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Runs the command; its exit code (see `refusal` for the codes errors get).
fn run() -> Result<i32> {
    // Blob buffers are short-lived and often large. glibc raises its mmap threshold
    // after such frees, after which big buffers come from arenas that are never
    // returned to the OS (~600 MB retained after one pass over chromium). A fixed
    // threshold keeps large buffers mmapped, so freeing them returns the memory.
    unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024) };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,fuser=error"))
        .init();
    let cli = Cli::parse();
    // As `git -C`: everything (relative paths too) as if started there.
    if let Some(dir) = &cli.dir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("-C {}: can't change to it", dir.display()))?;
    }
    match cli.command {
        Command::Mount(args) => cli::mount::mount(args)?,
        Command::Unmount { mountpoint } => cli::mount::unmount(&mountpoint)?,
        Command::Version { mountpoint } => return cli::mount::version(mountpoint.as_deref()),
        // As hooks, they find the repository from Claude Code's input.
        Command::Branch(args) if args.claude_hook.is_some() => cli::claude::worktree_create()?,
        Command::Branch(args) => cli::branch::branch(&cli::repo()?, args)?,
        Command::List(args) => cli::list::list(&cli::repo()?, args)?,
        Command::Changes(args) => cli::list::changes(&cli::repo()?, args)?,
        Command::Overlaps(args) => return cli::overlaps::overlaps(&cli::repo()?, args),
        Command::Watch(args) => cli::inbox::watch(&cli::repo()?, args)?,
        // Finds the repository itself: as a hook, none is no error.
        Command::Inbox(args) => return cli::inbox::inbox(args),
        Command::Send(args) => cli::inbox::send(&cli::repo()?, args)?,
        Command::Snapshot(args) if args.claude_hook.is_some() => cli::snapshot::snapshot_hook()?,
        Command::Snapshot(args) => cli::snapshot::snapshot(&cli::repo()?, args)?,
        Command::Layer(LayerArgs {
            claude_hook: Some(event),
            ..
        }) => cli::layer::layer_hook(&event)?,
        Command::Layer(args) => cli::layer::layer(&cli::repo()?, args)?,
        Command::Propose(args) => cli::proposal::propose(&cli::repo()?, args)?,
        Command::Proposals(args) => cli::proposal::proposals(&cli::repo()?, args)?,
        Command::Consent(args) => return cli::proposal::consent(&cli::repo()?, args),
        Command::Accept(args) => cli::proposal::accept(&cli::repo()?, args)?,
        Command::Reject(args) => cli::proposal::reject(&cli::repo()?, args)?,
        Command::Remove(args) if args.claude_hook.is_some() => cli::claude::worktree_remove()?,
        Command::Remove(args) => cli::remove::remove(&cli::repo()?, args)?,
        Command::Prune => cli::remove::prune(&cli::repo()?)?,
        Command::Prewarm(args) => cli::branch::prewarm(&cli::repo()?, args)?,
        Command::TrackReads(args) if args.claude_hook.is_some() => cli::claude::reads_hook()?,
        Command::TrackReads(args) => cli::branch::track_reads(&cli::repo()?, args)?,
        Command::PrewarmWalk {
            root,
            mode,
            threads,
        } => prewarm::run(root, mode, threads)?,
        Command::FilterProcess { common_dir, id } => daemon::filter::relay(&common_dir, &id)?,
        Command::Guard { hook, admin } => {
            if let Err(e) = git::guard::run_hook(hook, &admin, std::io::stdin().lock()) {
                eprintln!("{e:#}");
                return Ok(1);
            }
        }
        Command::BenchAttrs { repo, rev } => {
            git::init(&repo)?;
            bench::attrs::run(&rev)?;
        }
        Command::BenchRead { repo, rev, threads } => {
            git::init(&repo)?;
            bench::read::run(&rev, &threads)?;
        }
    }
    Ok(0)
}
