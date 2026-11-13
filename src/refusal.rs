//! Why a command refused, as a typed error: each kind has its own exit code,
//! and survives the control socket (see `daemon::control`).
//!
//! Exit codes, for every command: 0 success, 1 any other error, 2 a usage
//! error, 3 not found (`NotFound`), 4 changes or git busy (`Changes`), 5 in
//! use or busy (`InUse`). Three commands give codes of their own:
//! `version <MOUNTPOINT>` 3 another build, 4 a daemon too old to tell, 5 no
//! answer; `overlaps` 6 something overlaps; `inbox --claude-hook rewake` 2
//! to wake the session (what Claude Code asks for).

/// Why a command refused. Each has its own exit code, so scripts can tell them
/// apart (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Refusal {
    /// No such workspace or snapshot (or not a valid name).
    NotFound,
    /// Uncommitted or staged changes, or a git operation in progress.
    Changes,
    /// A process uses it, or it is busy (being removed, unmounting).
    InUse,
}

impl Refusal {
    pub const ALL: [Refusal; 3] = [Refusal::NotFound, Refusal::Changes, Refusal::InUse];

    pub fn exit_code(self) -> i32 {
        match self {
            Refusal::NotFound => 3,
            Refusal::Changes => 4,
            Refusal::InUse => 5,
        }
    }

    /// Its name in the control socket's answers.
    pub fn name(self) -> &'static str {
        match self {
            Refusal::NotFound => "not-found",
            Refusal::Changes => "changes",
            Refusal::InUse => "in-use",
        }
    }

    /// An error with this refusal and `message`.
    pub fn error(self, message: impl std::fmt::Display) -> anyhow::Error {
        anyhow::Error::new(Refused {
            refusal: self,
            message: message.to_string(),
        })
    }

    /// The refusal `e` carries, if any.
    pub fn of(e: &anyhow::Error) -> Option<Refusal> {
        e.chain()
            .find_map(|c| c.downcast_ref::<Refused>())
            .map(|r| r.refusal)
    }
}

#[derive(Debug)]
struct Refused {
    refusal: Refusal,
    message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}
