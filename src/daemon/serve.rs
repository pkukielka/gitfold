//! The daemon in the foreground (`gitfold mount --foreground`, which a
//! background mount runs too): mounting the repository with FUSE, answering
//! control requests, until unmounted.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use fuser::{Config, MountOption};

use super::control::{self, Request};
use super::{commit_graph, follow};
use crate::fs::{self, GitFold, NameChanges};
use crate::mount_table;
use crate::prewarm::{self, Prewarm};

/// What `gitfold mount` says about the daemon (see its options there).
pub struct Options {
    pub repo: PathBuf,
    /// Checked (`daemon::check`).
    pub mountpoint: PathBuf,
    pub threads: Option<usize>,
    pub kernel_open: bool,
    pub prewarm: Prewarm,
    pub prewarm_threads: Option<usize>,
    pub prewarm_walks: u64,
    pub track_reads: bool,
    /// The flags it was given, to record once mounted for mounting it again
    /// the same way (`cli::mount::mount_again`); none if `gitfold mount`
    /// started it, which records them itself.
    pub flags: Option<Vec<String>>,
}

/// Mounts `repo` at `mountpoint` and serves it until unmounted.
pub fn run(options: Options) -> Result<()> {
    let Options {
        repo,
        mountpoint,
        threads,
        kernel_open,
        prewarm,
        prewarm_threads,
        prewarm_walks,
        track_reads,
        flags,
    } = options;
    // Children (git, prewarm walkers) are waited for: an ignored SIGCHLD
    // inherited from whatever started the daemon would have the kernel reap
    // them first.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
    super::unmount_on_signal(mountpoint.clone());
    prewarm::set_max_walks(prewarm_walks as usize);
    // Absolute: the repository handle keeps the path it was opened with.
    let common_dir = crate::git::init(&std::path::absolute(&repo)?)?;
    // Paths are resolved: don't keep the caller's directory busy (it may be
    // on another mount the user wants to unmount).
    std::env::set_current_dir("/")?;
    // Held until the daemon exits (see `crate::lock::mount_lock`).
    let _lock = crate::lock::mount_lock(&common_dir)?;
    log::info!(
        "gitfold {}: mounting {} at {}",
        crate::VERSION,
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
        MountOption::FSName(mount_table::fs_name(&common_dir)),
        MountOption::Subtype("gitfold".into()),
    ];
    // 0 as unset: fuser serves with no fewer than one.
    let request_threads = threads.filter(|&n| n > 0).unwrap_or(cpus);
    config.n_threads = Some(request_threads);
    config.clone_fd = true;
    let names = Arc::new(NameChanges::default());
    let fs = GitFold::new(
        common_dir.clone(),
        mountpoint.clone(),
        fs::Options {
            no_open: !kernel_open,
            prewarm,
            prewarm_threads: prewarm_threads.unwrap_or(prewarm::THREADS.min(cpus / 2).max(1)),
            track_reads,
            request_threads,
        },
        names.clone(),
    );
    // Before mounting, so `remove` finds it once the mount is up. Without it
    // commands can't reach the daemon (`remove`, `branch -w`, `version`): not
    // mounted.
    let listener = control::bind(&common_dir)?;
    let removal = fs.removal();
    let workspaces = fs.workspaces();
    let following = fs.workspaces();
    let mounted = mountpoint.clone();
    let session = fuser::Session::new(fs, &mountpoint, &config)?;
    // Mounted: where and how commands mount it again (`cli::mount::mount_again`).
    if let Some(flags) = &flags
        && let Err(e) = crate::settings::set_mountpoint(&common_dir, &mountpoint, flags)
    {
        log::warn!("recording the mount point in the git config: {e:#}");
    }
    names.set_notifier(session.notifier());
    // After the notifier is set: a removal tells the kernel.
    control::serve(listener, move |request| match request {
        Request::Remove { name, force, check } => match check {
            true => removal.check(&name, force),
            false => removal
                .remove(&name, force)
                .inspect(|done| log::info!("{done}")),
        },
        Request::Version => Ok(crate::VERSION.to_string()),
        Request::ResumePrewarm => {
            prewarm::resume();
            log::info!("prewarm resumed: the unmount didn't go ahead");
            Ok("prewarm resumed".into())
        }
        Request::Prewarm { branch } => Ok(workspaces.prewarm_now(&branch)),
        Request::TrackReads { branch } => workspaces.track_reads_now(&branch),
        Request::RecordReads { paths } => Ok(workspaces.record_reads(paths)),
        Request::Fork { from, to } => workspaces
            .fork(&from, &to)
            .inspect(|done| log::info!("{done}")),
        Request::StopPrewarm => {
            prewarm::stop_for_unmount(mounted.clone());
            log::info!("prewarm stopped for the unmount");
            Ok("prewarm stopped for the unmount".into())
        }
    })?;
    // Once mounted: a move runs git through the mount.
    follow::spawn(common_dir.clone(), following);
    commit_graph::spawn(common_dir.clone());
    session.run()?;
    Ok(())
}
