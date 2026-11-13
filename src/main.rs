mod bench;
mod cli;
mod daemon;
mod fs;
mod git;
mod inode;
mod refusal;
mod store;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::builder::BoolishValueParser;
use clap::{Parser, Subcommand};
use fuser::{Config, MountOption};

use daemon::{control, prewarm};

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
    Mount {
        /// The repository: its top-level work tree or its git directory.
        repo: PathBuf,
        mountpoint: PathBuf,
        /// Stay in the foreground, logging to stderr (for service managers).
        #[arg(long)]
        foreground: bool,
        /// Log file in the background (default:
        /// ~/.local/state/gitfold/<mount point>.log).
        #[arg(long)]
        log: Option<PathBuf>,
        /// FUSE worker threads (default: number of CPUs).
        #[arg(long)]
        threads: Option<usize>,
        /// Have the kernel send open/release for every file (slower; for comparison).
        #[arg(long)]
        kernel_open: bool,
        /// Populate kernel caches in the background after a snapshot or workspace is opened.
        #[arg(long, value_enum, default_value = "meta")]
        prewarm: prewarm::Prewarm,
        /// Prewarm threads per walk (default: 4, or half the CPUs if fewer).
        #[arg(long)]
        prewarm_threads: Option<usize>,
        /// Walks running at once; the others wait.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
        prewarm_walks: u64,
        /// Record the files agents read in workspaces, for `gitfold overlaps`
        /// (branches can say otherwise: `gitfold track-reads`). A tracked
        /// workspace's prewarm `content` walks as `meta`: its walk would read
        /// every file before the agent.
        #[arg(long)]
        track_reads: bool,
    },
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
    Branch {
        branch: String,
        /// Copy the uncommitted changes of the workspace --from names, or of the
        /// one you are in.
        #[arg(short = 'w', long)]
        with_changes: bool,
        /// Start point (default: HEAD of the worktree you are in, as for `git
        /// branch`).
        #[arg(long)]
        from: Option<String>,
        /// Prewarm mode for this workspace, kept across remounts (in git
        /// config, branch.<BRANCH>.gitfoldPrewarm); a mount's `--prewarm none`
        /// still wins.
        #[arg(long, value_enum)]
        prewarm: Option<prewarm::Prewarm>,
        /// Record the files read in this workspace, for `gitfold overlaps`, or
        /// not, whatever the mount's --track-reads says (in git config,
        /// branch.<BRANCH>.gitfoldTrackReads).
        #[arg(long, value_name = "on|off", value_parser = BoolishValueParser::new())]
        track_reads: Option<bool>,
    },
    /// List the workspaces and their changes.
    ///
    /// For each: how many files it changed and deleted, how many ignored files
    /// it holds, its base commit, and notes (branch deleted, base behind).
    List,
    /// List a workspace's uncommitted changes (A/M/D per path), without git status.
    ///
    /// Against its base, untracked files included and ignored ones not, whatever
    /// is staged (as `git add -A` and `git diff --cached --name-status`): one
    /// path per line, `A` added, `M` modified, `D` deleted, quoted as git quotes
    /// paths. Read from the workspace's stored state, mounted or not.
    Changes {
        /// The workspace's branch (default: the workspace you are in).
        branch: Option<String>,
        /// End each line with NUL instead of a newline, paths unquoted.
        #[arg(short = 'z')]
        nul: bool,
    },
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
    Overlaps {
        /// The workspace's branch (default: the workspace you are in).
        branch: Option<String>,
        /// What to compare with (repeatable; default: origin/HEAD, the remote's
        /// default branch, else main).
        #[arg(long, value_name = "REV")]
        against: Vec<String>,
        /// Compare with every other workspace (instead of the default target;
        /// with --against, as well).
        #[arg(long)]
        against_workspaces: bool,
        /// Only files read.
        #[arg(long, conflicts_with = "writes")]
        reads: bool,
        /// Only paths changed.
        #[arg(long)]
        writes: bool,
        /// End each line with NUL instead of a newline, paths unquoted.
        #[arg(short = 'z')]
        nul: bool,
    },
    /// Delete a workspace, or a snapshot's registration.
    ///
    /// A workspace: its registration, stored changes and base ref (its commits
    /// stay on the branch). A snapshot (@<rev>): its registration. Works
    /// mounted too, unless it is in use. Exit codes: 3 not found, 4 uncommitted
    /// changes (or a git operation in progress), 5 in use, 1 other errors, 2
    /// usage errors.
    Remove {
        /// A workspace's branch, or @<rev> for a snapshot.
        #[arg(value_name = "BRANCH|@REV")]
        name: String,
        /// Discard uncommitted changes.
        #[arg(long)]
        force: bool,
        /// Only check whether it would be removed; remove nothing.
        #[arg(long)]
        check: bool,
        /// While in use, retry for up to this many seconds.
        #[arg(long, value_name = "SECS", default_value_t = 0)]
        wait: u64,
    },
    /// Delete the snapshot registrations.
    ///
    /// They are registered again when opened. The repository must not be
    /// mounted.
    Prune,
    /// Set a workspace's prewarm mode.
    ///
    /// As `branch --prewarm`, kept in git config; while mounted also applied at
    /// once: the workspace is walked if open, or its walk stopped (none).
    Prewarm {
        branch: String,
        #[arg(value_enum)]
        mode: prewarm::Prewarm,
    },
    /// Record the files read in a workspace, for `gitfold overlaps`, or stop.
    ///
    /// As `branch --track-reads`, kept in git config, whatever the mount's
    /// --track-reads says; while mounted also applied at once. Files read
    /// before tracking starts aren't seen (the kernel caches them). Stopping
    /// keeps what was recorded.
    TrackReads {
        branch: String,
        #[arg(value_name = "on|off", value_parser = BoolishValueParser::new(), action = clap::ArgAction::Set)]
        on: bool,
    },
    /// Walk a freshly opened snapshot or workspace (started by the daemon).
    #[command(hide = true)]
    PrewarmWalk {
        root: PathBuf,
        #[arg(long, value_enum)]
        mode: prewarm::Prewarm,
        #[arg(long)]
        threads: usize,
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

fn main() -> Result<()> {
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
    // The repository commands work on: the one the current directory is in.
    let repo = || git::discover(std::path::Path::new("."));
    match cli.command {
        Command::Mount {
            repo,
            mountpoint,
            foreground,
            log,
            threads,
            kernel_open,
            prewarm,
            prewarm_threads,
            prewarm_walks,
            track_reads,
        } => {
            let mountpoint = daemon::check(&mountpoint)?;
            if !foreground {
                let log = match log {
                    Some(log) => std::path::absolute(log)?,
                    None => cli::mount::default_log(&mountpoint)?,
                };
                cli::mount::start(&mountpoint, &log)?;
                println!(
                    "mounted at {} (log: {})",
                    mountpoint.display(),
                    log.display()
                );
                println!("unmount with: gitfold unmount {}", mountpoint.display());
                return Ok(());
            }
            daemon::unmount_on_signal(mountpoint.clone());
            prewarm::set_max_walks(prewarm_walks as usize);
            // Absolute: the repository handle keeps the path it was opened with.
            let common_dir = git::init(&std::path::absolute(&repo)?)?;
            // Paths are resolved: don't keep the caller's directory busy (it may be
            // on another mount the user wants to unmount).
            std::env::set_current_dir("/")?;
            // Held until the daemon exits (see `git::lock::mount_lock`).
            let _lock = git::lock::mount_lock(&common_dir)?;
            log::info!(
                "gitfold {VERSION}: mounting {} at {}",
                common_dir.display(),
                mountpoint.display()
            );
            let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
            let mut config = Config::default();
            config.mount_options = vec![
                // Read-write: workspaces are writable; snapshots have no write bits, and
                // answer EROFS for what the kernel lets through.
                MountOption::DefaultPermissions,
                // The common dir, for people reading the mount table (`remove` detects
                // mounts by the lock).
                MountOption::FSName(daemon::fs_name(&common_dir)),
                MountOption::Subtype("gitfold".into()),
            ];
            config.n_threads = Some(threads.unwrap_or(cpus));
            config.clone_fd = true;
            let names = std::sync::Arc::new(fs::NameChanges::default());
            let fs = fs::GitFold::new(
                common_dir.clone(),
                mountpoint.clone(),
                fs::Options {
                    no_open: !kernel_open,
                    prewarm,
                    prewarm_threads: prewarm_threads
                        .unwrap_or(prewarm::THREADS.min(cpus / 2).max(1)),
                    track_reads,
                },
                names.clone(),
            );
            // Before mounting, so `remove` finds it once the mount is up. Without it
            // only removals need an unmount.
            let listener = control::bind(&common_dir)
                .inspect_err(|e| log::warn!("{e:#}; `gitfold remove` will need an unmount"))
                .ok();
            let removal = fs.removal();
            let workspaces = fs.workspaces();
            let mounted = mountpoint.clone();
            let session = fuser::Session::new(fs, &mountpoint, &config)?;
            names.set_notifier(session.notifier());
            // After the notifier is set: a removal tells the kernel.
            if let Some(listener) = listener {
                control::serve(listener, move |request| match request {
                    control::Request::Remove { name, force, check } => match check {
                        true => removal.check(&name, force),
                        false => removal
                            .remove(&name, force)
                            .inspect(|done| log::info!("{done}")),
                    },
                    control::Request::Version => Ok(VERSION.to_string()),
                    control::Request::ResumePrewarm => {
                        prewarm::resume();
                        log::info!("prewarm resumed: the unmount didn't go ahead");
                        Ok("prewarm resumed".into())
                    }
                    control::Request::Prewarm { branch } => Ok(workspaces.prewarm_now(&branch)),
                    control::Request::TrackReads { branch } => workspaces.track_reads_now(&branch),
                    control::Request::Fork { from, to } => workspaces
                        .fork(&from, &to)
                        .inspect(|done| log::info!("{done}")),
                    control::Request::StopPrewarm => {
                        prewarm::stop_for_unmount(mounted.clone());
                        log::info!("prewarm stopped for the unmount");
                        Ok("prewarm stopped for the unmount".into())
                    }
                })?;
            }
            session.run()?;
        }
        Command::Unmount { mountpoint } => cli::mount::unmount(&mountpoint)?,
        Command::Version { mountpoint } => match cli::mount::version(mountpoint.as_deref())? {
            0 => {}
            code => std::process::exit(code),
        },
        Command::Branch {
            branch,
            from,
            prewarm,
            track_reads,
            with_changes,
        } => {
            let repo = repo()?;
            or_exit(cli::manage::branch(
                &repo,
                &branch,
                from.as_deref(),
                prewarm,
                track_reads,
                with_changes,
            ));
        }
        Command::List => cli::manage::list(&repo()?)?,
        Command::Changes { branch, nul } => {
            let repo = repo()?;
            let branch = cli::branch_or_current(branch, &repo)?;
            cli::manage::changes(&repo, &branch, nul)?
        }
        Command::Overlaps {
            branch,
            against,
            against_workspaces,
            reads,
            writes,
            nul,
        } => {
            let kinds = match (reads, writes) {
                (true, _) => cli::overlaps::Kinds::Reads,
                (_, true) => cli::overlaps::Kinds::Writes,
                _ => cli::overlaps::Kinds::Both,
            };
            let found = (|| {
                let repo = repo()?;
                let branch = cli::branch_or_current(branch, &repo)?;
                cli::overlaps::overlaps(&repo, &branch, &against, against_workspaces, kinds, nul)
            })();
            match found {
                Ok(true) => std::process::exit(cli::overlaps::FOUND),
                Ok(false) => {}
                Err(e) => or_exit(Err(e)),
            }
        }
        Command::Remove {
            name,
            force,
            check,
            wait,
        } => {
            let wait = std::time::Duration::from_secs(wait);
            or_exit(cli::manage::remove(&repo()?, &name, force, check, wait));
        }
        Command::Prune => cli::manage::prune(&repo()?)?,
        Command::Prewarm { branch, mode } => cli::manage::prewarm(&repo()?, &branch, mode)?,
        Command::TrackReads { branch, on } => cli::manage::track_reads(&repo()?, &branch, on)?,
        Command::PrewarmWalk {
            root,
            mode,
            threads,
        } => prewarm::run(root, mode, threads)?,
        Command::BenchAttrs { repo, rev } => {
            git::init(&repo)?;
            bench::attrs::run(&rev)?;
        }
        Command::BenchRead { repo, rev, threads } => {
            git::init(&repo)?;
            bench::read::run(&rev, &threads)?;
        }
    }
    Ok(())
}

/// Exits with the error's refusal code (see `refusal`), or 1, if `result` failed.
fn or_exit(result: Result<()>) {
    if let Err(e) = result {
        eprintln!("Error: {e:?}");
        std::process::exit(refusal::Refusal::of(&e).map_or(1, |r| r.exit_code()));
    }
}
