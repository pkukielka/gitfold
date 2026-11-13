//! The daemon in the foreground (`gitfold mount --foreground`, which a
//! background mount runs too): mounting the repository with FUSE, answering
//! control requests, until unmounted.

/// The arguments of `gitfold mount` (see `main`).
#[derive(clap::Args)]
pub struct MountArgs {
    /// The repository: its top-level work tree or its git directory.
    pub repo: std::path::PathBuf,
    pub mountpoint: std::path::PathBuf,
    /// Stay in the foreground, logging to stderr (for service managers).
    #[arg(long)]
    pub foreground: bool,
    /// Log file in the background (default:
    /// ~/.local/state/gitfold/<mount point>.log).
    #[arg(long)]
    pub log: Option<std::path::PathBuf>,
    /// FUSE worker threads (default: number of CPUs).
    #[arg(long)]
    pub threads: Option<usize>,
    /// Have the kernel send open/release for every file (slower; for comparison).
    #[arg(long)]
    pub kernel_open: bool,
    /// Populate kernel caches in the background after a snapshot or workspace is opened.
    #[arg(long, value_enum, default_value = "meta")]
    pub prewarm: crate::daemon::prewarm::Prewarm,
    /// Prewarm threads per walk (default: 4, or half the CPUs if fewer).
    #[arg(long)]
    pub prewarm_threads: Option<usize>,
    /// Walks running at once; the others wait.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
    pub prewarm_walks: u64,
    /// Record the files agents read in workspaces, for `gitfold overlaps`
    /// (branches can say otherwise: `gitfold track-reads`). A tracked
    /// workspace's prewarm `content` walks as `meta`: its walk would read
    /// every file before the agent.
    #[arg(long)]
    pub track_reads: bool,
}

/// Mounts `args.repo` at `mountpoint` (checked) and serves it until unmounted.
pub fn run(args: MountArgs, mountpoint: std::path::PathBuf) -> anyhow::Result<()> {
    use fuser::{Config, MountOption};
    let MountArgs {
        repo,
        threads,
        kernel_open,
        prewarm,
        prewarm_threads,
        prewarm_walks,
        track_reads,
        ..
    } = args;
    super::unmount_on_signal(mountpoint.clone());
    super::prewarm::set_max_walks(prewarm_walks as usize);
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
        MountOption::FSName(super::fs_name(&common_dir)),
        MountOption::Subtype("gitfold".into()),
    ];
    config.n_threads = Some(threads.unwrap_or(cpus));
    config.clone_fd = true;
    let names = std::sync::Arc::new(crate::fs::NameChanges::default());
    let fs = crate::fs::GitFold::new(
        common_dir.clone(),
        mountpoint.clone(),
        crate::fs::Options {
            no_open: !kernel_open,
            prewarm,
            prewarm_threads: prewarm_threads
                .unwrap_or(super::prewarm::THREADS.min(cpus / 2).max(1)),
            track_reads,
        },
        names.clone(),
    );
    // Before mounting, so `remove` finds it once the mount is up. Without it
    // only removals need an unmount.
    let listener = super::control::bind(&common_dir)
        .inspect_err(|e| log::warn!("{e:#}; `gitfold remove` will need an unmount"))
        .ok();
    let removal = fs.removal();
    let workspaces = fs.workspaces();
    let mounted = mountpoint.clone();
    let session = fuser::Session::new(fs, &mountpoint, &config)?;
    names.set_notifier(session.notifier());
    // After the notifier is set: a removal tells the kernel.
    if let Some(listener) = listener {
        super::control::serve(listener, move |request| match request {
            super::control::Request::Remove { name, force, check } => match check {
                true => removal.check(&name, force),
                false => removal
                    .remove(&name, force)
                    .inspect(|done| log::info!("{done}")),
            },
            super::control::Request::Version => Ok(crate::VERSION.to_string()),
            super::control::Request::ResumePrewarm => {
                super::prewarm::resume();
                log::info!("prewarm resumed: the unmount didn't go ahead");
                Ok("prewarm resumed".into())
            }
            super::control::Request::Prewarm { branch } => Ok(workspaces.prewarm_now(&branch)),
            super::control::Request::TrackReads { branch } => workspaces.track_reads_now(&branch),
            super::control::Request::Fork { from, to } => workspaces
                .fork(&from, &to)
                .inspect(|done| log::info!("{done}")),
            super::control::Request::StopPrewarm => {
                super::prewarm::stop_for_unmount(mounted.clone());
                log::info!("prewarm stopped for the unmount");
                Ok("prewarm stopped for the unmount".into())
            }
        })?;
    }
    session.run()?;
    Ok(())
}
