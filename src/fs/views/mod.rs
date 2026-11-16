//! The read-only views `<mount>/@live/N/` and `<mount>/@committed/N/`:
//! branch N merged with every branch built on it (layers-design/15-tree-spec.md
//! section 6 in the blog notes). `@committed` is served; `@live` is reserved
//! (its views come with the workspaces' uncommitted state).
//!
//! - `members`: which branches a view merges in, and the refs it is made of.
//! - `merge`: the merge, by `git merge-tree`, in a private repository.
//! - `shown`: the merged tree as directories and files, its summary, and what
//!   changed from the one before.
//! - `serve`: the views served, by slot, and their nodes.
//! - `watch`: keeping each view current, and telling the kernel.

pub mod members;
pub mod merge;
mod serve;
pub mod shown;
#[cfg(test)]
mod test_repo;
mod watch;

pub use serve::{Item, ViewDir, Views};
