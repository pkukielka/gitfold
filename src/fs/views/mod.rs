//! The read-only views `<mount>/@live/N/` and `<mount>/@committed/N/`:
//! branch N merged with every branch built on it (layers-design/15-tree-spec.md
//! section 6 in the blog notes): `@committed` of their tips, `@live` of their
//! workspaces as they are (uncommitted work included; a branch without one
//! as its tip).
//!
//! - `members`: which branches a view merges in, and the refs it is made of.
//! - `merge`: the merge, by `git merge-tree`, in a private repository.
//! - `live`: a workspace as it is, as a tree (`git add -A` on a copy of its
//!   index); `lives`: one per workspace, made again when it changed.
//! - `shown`: the merged tree as directories and files, its summary, and what
//!   changed from the one before.
//! - `serve`: the views served, by slot, and their nodes.
//! - `watch`: keeping each view current, and telling the kernel. A view not
//!   accessed for a minute rests until accessed again.
//! - `notices`: the conflict notices, when a workspace's branch gets new
//!   commits (the live trees and the private repository shared).

mod live;
mod lives;
pub mod members;
pub mod merge;
mod notices;
mod serve;
pub mod shown;
#[cfg(test)]
mod test_repo;
mod watch;

pub use notices::{Step, spawn as notices};
pub use serve::{Item, Kind, ViewDir, Views, waiting_at_most};
