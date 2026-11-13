mod bench;
mod cli;
mod daemon;
mod fs;
mod fsutil;
mod git;
mod inode;
mod lock;
mod refusal;
mod settings;
mod state;
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
    /// For each: how many files it changed and deleted (uncommitted), how many
    /// ignored files it holds, its base commit, how long ago it last changed
    /// (a file written, created or deleted, or a commit), and notes (branch
    /// deleted, base behind).
    List {
        /// Also the lines added and removed, in total (reads every changed
        /// file and its base version).
        #[arg(long)]
        stat: bool,
    },
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
        /// Also the lines added and removed per path (`-` for binary files),
        /// as `git diff --numstat`, and a summary line (not with -z).
        #[arg(long)]
        stat: bool,
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
    /// Wait for a change in a directory of a task folder, and print it.
    ///
    /// <TARGET> is <BRANCH>:<DIR>, or <DIR> of the workspace you are in (`.`
    /// for the folder itself). Waits until a file there is written, created,
    /// moved in, deleted or moved out (not below it; dot names don't count),
    /// prints `C` (changed) or `D` (deleted), a tab and the name for it and
    /// whatever else changed right after, and exits. Plain inotify on the
    /// folder's directory: mounted or not, whoever writes.
    Watch {
        target: String,
        /// Give up after this many seconds, printing nothing.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        /// One JSON object per line: {"event": "changed"|"deleted", "path"}.
        #[arg(long)]
        json: bool,
    },
    /// Take the messages in a workspace's inbox and print them.
    ///
    /// The inbox is `inbox/` in the task folder (`gitfold.inbox` names it). Each
    /// message is taken once, whoever else reads: printed as sent (headers, an
    /// empty line, the body), an empty line between messages, and deleted.
    Inbox {
        /// The workspace's branch (default: the workspace you are in).
        branch: Option<String>,
        /// If there are none, wait for one.
        #[arg(long)]
        wait: bool,
        /// With --wait: give up after this many seconds.
        #[arg(long, value_name = "SECS", requires = "wait")]
        timeout: Option<u64>,
        /// Only list them, taking nothing: sender and subject (or first line),
        /// one per line; nothing if there are none.
        #[arg(long, conflicts_with = "wait")]
        peek: bool,
        /// One JSON object per message: {"file", "headers", "body"}.
        #[arg(long)]
        json: bool,
        /// Hand them to a Claude Code hook: `user-prompt-submit` (as context),
        /// `post-tool-use` (as `additionalContext`), or `rewake` (an async
        /// `Stop` hook with `asyncRewake`: waits for one, then wakes the idle
        /// session). See skills/gitfold/SKILL.md.
        #[arg(long, value_enum, value_name = "EVENT", conflicts_with = "peek")]
        claude_hook: Option<cli::inbox::Hook>,
    },
    /// Leave a message in a workspace's inbox.
    ///
    /// Written atomically into <BRANCH>'s task folder: `from:` and `time:`
    /// headers, `subject:` if given, an empty line, then the text (`-`: read
    /// from stdin). The branch needs no workspace yet. Exit codes: 3 no such
    /// branch, 1 other errors.
    Send {
        /// The recipient's branch.
        to: String,
        /// The message (`-`: read it from stdin).
        #[arg(required = true)]
        text: Vec<String>,
        /// Who it is from (default: the workspace you are in, else `user`).
        #[arg(long)]
        from: Option<String>,
        #[arg(short, long)]
        subject: Option<String>,
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
                return Ok(0);
            }
            daemon::unmount_on_signal(mountpoint.clone());
            prewarm::set_max_walks(prewarm_walks as usize);
            // Absolute: the repository handle keeps the path it was opened with.
            let common_dir = git::init(&std::path::absolute(&repo)?)?;
            // Paths are resolved: don't keep the caller's directory busy (it may be
            // on another mount the user wants to unmount).
            std::env::set_current_dir("/")?;
            // Held until the daemon exits (see `lock::mount_lock`).
            let _lock = lock::mount_lock(&common_dir)?;
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
        Command::Version { mountpoint } => return cli::mount::version(mountpoint.as_deref()),
        Command::Branch {
            branch,
            from,
            prewarm,
            track_reads,
            with_changes,
        } => {
            let repo = repo()?;
            cli::branch::branch(
                &repo,
                &branch,
                from.as_deref(),
                prewarm,
                track_reads,
                with_changes,
            )?;
        }
        Command::List { stat } => cli::list::list(&repo()?, stat)?,
        Command::Changes { branch, nul, stat } => {
            let repo = repo()?;
            let branch = cli::branch_or_current(branch, &repo)?;
            cli::list::changes(&repo, &branch, nul, stat)?
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
            let repo = repo()?;
            let branch = cli::branch_or_current(branch, &repo)?;
            if cli::overlaps::overlaps(&repo, &branch, &against, against_workspaces, kinds, nul)? {
                return Ok(cli::overlaps::FOUND);
            }
        }
        Command::Watch {
            target,
            timeout,
            json,
        } => {
            let repo = repo()?;
            let current = cli::branch_or_current(None, &repo).ok();
            let target = if target == "." { String::new() } else { target };
            cli::inbox::watch(&repo, current.as_deref(), &target, timeout, json)?;
        }
        Command::Inbox {
            branch,
            wait,
            timeout,
            peek,
            json,
            claude_hook,
        } => {
            // A hook runs in every session: outside a workspace, nothing to do.
            let found = repo().and_then(|repo| Ok((cli::branch_or_current(branch, &repo)?, repo)));
            let (branch, repo) = match found {
                Ok(found) => found,
                Err(_) if claude_hook.is_some() => return Ok(0),
                Err(e) => return Err(e),
            };
            let args = cli::inbox::InboxArgs {
                wait,
                timeout,
                peek,
                json,
                hook: claude_hook,
            };
            match cli::inbox::inbox(&repo, &branch, args) {
                Ok(code) => return Ok(code),
                // A hook's error would show on every tool call: logged, not raised.
                Err(e) if claude_hook.is_some() => log::warn!("gitfold inbox: {e:#}"),
                Err(e) => return Err(e),
            }
        }
        Command::Send {
            to,
            text,
            from,
            subject,
        } => {
            let repo = repo()?;
            let from = from
                .or_else(|| cli::branch_or_current(None, &repo).ok())
                .unwrap_or_else(|| "user".to_string());
            let body = match text.as_slice() {
                [dash] if dash == "-" => std::io::read_to_string(std::io::stdin())?,
                words => words.join(" "),
            };
            cli::inbox::send(&repo, &to, &from, subject.as_deref(), &body)?;
        }
        Command::Remove {
            name,
            force,
            check,
            wait,
        } => {
            let wait = std::time::Duration::from_secs(wait);
            cli::remove::remove(&repo()?, &name, force, check, wait)?;
        }
        Command::Prune => cli::remove::prune(&repo()?)?,
        Command::Prewarm { branch, mode } => cli::branch::prewarm(&repo()?, &branch, mode)?,
        Command::TrackReads { branch, on } => cli::branch::track_reads(&repo()?, &branch, on)?,
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
    Ok(0)
}
