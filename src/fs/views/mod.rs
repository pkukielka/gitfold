//! The read-only views `<mount>/@live/N/` and `<mount>/@committed/N/`:
//! branch N merged with every branch built on it (layers-design/15-tree-spec.md
//! section 6 in the blog notes).
//!
//! - `members`: which branches a view merges in, and the committed view's
//!   inputs.
//! - `build`: a view's content, the members' changes merged path by path.
//! - `merge`: the content merge of one file.

pub mod build;
pub mod members;
pub mod merge;
#[cfg(test)]
mod test_repo;
