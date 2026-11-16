//! The three-way merge of one file's content, as `git merge-file` does it:
//! conflicts kept in the file with git's markers, named by branch. All a view
//! needs of a content merge (`build`), so another merge can replace it.

use std::path::Path;

use gix::merge::blob::Resolution;
use gix::merge::blob::builtin_driver::{self, text};

pub use text::ConflictStyle;

/// What a merge made.
#[derive(Debug, PartialEq)]
pub enum Merged {
    Clean(Vec<u8>),
    /// With conflicts kept as markers.
    Markers(Vec<u8>),
    /// Nothing: a side is binary (as git decides it).
    Binary,
}

/// The merge of `ours` and `theirs`, both changed from `base`: conflicting
/// lines kept between git's markers, named `ours_label` and `theirs_label`
/// (the base `base`, shown in the diff3 styles), in `style`.
pub fn merge3(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    ours_label: &str,
    theirs_label: &str,
    style: ConflictStyle,
) -> Merged {
    if [base, ours, theirs].iter().any(|d| is_binary(d)) {
        return Merged::Binary;
    }
    let mut input = gix::diff::blob::InternedInput::default();
    let mut data = Vec::new();
    let labels = text::Labels {
        ancestor: Some("base".into()),
        current: Some(ours_label.into()),
        other: Some(theirs_label.into()),
    };
    let options = text::Options {
        conflict: text::Conflict::Keep {
            style,
            marker_size: text::Conflict::DEFAULT_MARKER_SIZE
                .try_into()
                .expect("non-zero"),
        },
        // As git's merge (merge-ort) diffs, not `git merge-file` (Myers).
        diff_algorithm: gix::diff::blob::Algorithm::Histogram,
    };
    match builtin_driver::text(&mut data, &mut input, labels, ours, base, theirs, options) {
        Resolution::Conflict => Merged::Markers(data),
        Resolution::Complete | Resolution::CompleteWithAutoResolvedConflict => Merged::Clean(data),
    }
}

/// Whether git takes `data` for binary: a NUL in its first 8000 bytes.
fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// The conflict style git's config for the repository of `common_dir` asks
/// for now (`merge.conflictStyle`: `merge`, the default, `diff3` or
/// `zdiff3`), as git itself reads it (the user's config included, which the
/// daemon's repository handle has as it was at mount time).
pub fn conflict_style(common_dir: &Path) -> ConflictStyle {
    let args = ["config", "--get", "merge.conflictStyle"];
    // Unset: git exits 1.
    match crate::git::cmd::output(common_dir, &args).ok().as_deref() {
        Some(b"diff3\n") => ConflictStyle::Diff3,
        Some(b"zdiff3\n") => ConflictStyle::ZealousDiff3,
        _ => ConflictStyle::Merge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(merged: Merged) -> String {
        match merged {
            Merged::Clean(data) | Merged::Markers(data) => String::from_utf8(data).unwrap(),
            Merged::Binary => panic!("binary"),
        }
    }

    #[test]
    fn clean_and_conflicting() {
        let base = b"a\nb\nc\n";
        let style = ConflictStyle::Merge;
        let m = merge3(base, b"A\nb\nc\n", b"a\nb\nC\n", "api", "ui", style);
        assert_eq!(m, Merged::Clean(b"A\nb\nC\n".to_vec()));

        let m = merge3(base, b"a\nX\nc\n", b"a\nY\nc\n", "api", "ui", style);
        assert!(matches!(m, Merged::Markers(_)));
        assert_eq!(
            text(m),
            "a\n<<<<<<< api\nX\n=======\nY\n>>>>>>> ui\nc\n",
            "git's markers, named by branch"
        );

        let style = ConflictStyle::Diff3;
        let m = merge3(base, b"a\nX\nc\n", b"a\nY\nc\n", "api", "ui", style);
        let text = text(m);
        assert!(text.contains("||||||| base\nb\n"), "{text}");
    }

    #[test]
    fn style_as_configured() {
        use super::super::test_repo::{config, git_dir};
        // The only test setting it: others pass their style.
        config("merge.conflictStyle", "zdiff3");
        assert_eq!(conflict_style(&git_dir()), ConflictStyle::ZealousDiff3);
        config("merge.conflictStyle", "merge");
        assert_eq!(conflict_style(&git_dir()), ConflictStyle::Merge);
    }

    #[test]
    fn binary() {
        let style = ConflictStyle::Merge;
        assert_eq!(merge3(b"a", b"a\0b", b"c", "", "", style), Merged::Binary);
        assert!(!is_binary(b"text\n"));
        let mut late = vec![b'a'; 8000];
        late.push(0);
        assert!(!is_binary(&late), "only the first 8000 bytes count");
    }
}
