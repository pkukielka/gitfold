mod admin;
mod benchattrs;
mod benchread;
mod fs;
mod git;
mod inode;
mod mountpoint;
mod prewarm;
mod store;
mod worktree;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use fuser::{Config, MountOption};

#[derive(Parser)]
#[command(about = "Git revisions as a filesystem")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a repository: revisions as <mountpoint>/@<rev>/..., branches as <mountpoint>/<branch>/...
    Mount {
        repo: PathBuf,
        mountpoint: PathBuf,
        /// FUSE worker threads (default: number of CPUs).
        #[arg(long)]
        threads: Option<usize>,
        /// Have the kernel send open/release for every file (slower; for comparison).
        #[arg(long)]
        kernel_open: bool,
        /// Populate kernel caches in the background after a snapshot or workspace is opened.
        #[arg(long, value_enum, default_value = "meta")]
        prewarm: prewarm::Prewarm,
        /// Prewarm threads (default: half the CPUs).
        #[arg(long)]
        prewarm_threads: Option<usize>,
    },
    /// Create a branch to open as a workspace (<mountpoint>/<name>).
    Branch {
        repo: PathBuf,
        name: String,
        /// Start point (default: the current HEAD).
        #[arg(long)]
        from: Option<String>,
    },
    /// List the repository's workspaces and their uncommitted changes.
    List { repo: PathBuf },
    /// Delete a workspace: its registration, stored changes and base ref.
    /// Its commits stay on the branch. The repository must not be mounted.
    Remove {
        repo: PathBuf,
        branch: String,
        /// Discard uncommitted changes.
        #[arg(long)]
        force: bool,
    },
    /// Delete the snapshot registrations (recreated when opened again). The
    /// repository must not be mounted.
    Prune { repo: PathBuf },
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

fn main() -> Result<()> {
    // Blob buffers are short-lived and often large. glibc raises its mmap threshold
    // after such frees, after which big buffers come from arenas that are never
    // returned to the OS (~600 MB retained after one pass over chromium). A fixed
    // threshold keeps large buffers mmapped, so freeing them returns the memory.
    unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024) };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match Cli::parse().command {
        Command::Mount {
            repo,
            mountpoint,
            threads,
            kernel_open,
            prewarm,
            prewarm_threads,
        } => {
            let mountpoint = mountpoint::check(&mountpoint)?;
            mountpoint::unmount_on_signal(mountpoint.clone());
            let common_dir = git::init(&repo)?;
            // Held until the daemon exits (see `admin::mount_lock`).
            let _lock = admin::mount_lock(&common_dir)?;
            let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
            let mut config = Config::default();
            config.mount_options = vec![
                // Read-write: workspaces are writable; snapshots have no write bits, and
                // answer EROFS for what the kernel lets through.
                MountOption::DefaultPermissions,
                // The common dir, for people reading the mount table (`remove` detects
                // mounts by the lock).
                MountOption::FSName(admin::fs_name(&common_dir)),
                MountOption::Subtype("gitfold".into()),
            ];
            config.n_threads = Some(threads.unwrap_or(cpus));
            config.clone_fd = true;
            let fs = fs::GitFold::new(
                common_dir,
                mountpoint.clone(),
                fs::Options {
                    no_open: !kernel_open,
                    prewarm,
                    prewarm_threads: prewarm_threads.unwrap_or((cpus / 2).max(1)),
                },
            );
            fuser::mount(fs, &mountpoint, &config)?;
        }
        Command::Branch { repo, name, from } => admin::branch(&repo, &name, from.as_deref())?,
        Command::List { repo } => admin::list(&repo)?,
        Command::Remove {
            repo,
            branch,
            force,
        } => admin::remove(&repo, &branch, force)?,
        Command::Prune { repo } => admin::prune(&repo)?,
        Command::BenchAttrs { repo, rev } => {
            git::init(&repo)?;
            benchattrs::run(&rev)?;
        }
        Command::BenchRead { repo, rev, threads } => {
            git::init(&repo)?;
            benchread::run(&rev, &threads)?;
        }
    }
    Ok(())
}
