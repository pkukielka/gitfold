//! The views' merge, by git itself: `git merge-tree --write-tree` folds each
//! member into the view so far, against its merge point with its parent.
//! Renames, `.gitattributes` merge settings, merge drivers and
//! `merge.conflictStyle` are git's; conflicts stay in the files as git leaves
//! them, and what git says of them is the view's summary.
//!
//! git runs in the repository itself (its settings, attributes and drivers,
//! in its work tree's top), with only its objects going elsewhere: into a
//! private bare repository, `<git dir>/gitfold/.views.git`, which borrows the
//! repository's objects (`objects/info/alternates`) and which gitfold reads
//! too (`git::open_private`). Nothing is written into the repository. The
//! first fold's conflict markers name the branches (`<<<<<<< api`,
//! `>>>>>>> ui`); later ones name the view so far by its tree. The private
//! repository's only refs are the view's: the tree it shows
//! (`refs/gitfold/views/<view>`) and, until it shows it, the new one
//! (`refs/gitfold/next/<view>`). It is made anew at each mount, and
//! garbage-collected as git's `gc --auto` decides (more loose objects than
//! `gc.auto`'s default): what no view shows goes. Missing objects aren't
//! fetched: in a partial clone, gitfold fetches what a merge needs first, as
//! for a snapshot.

use std::collections::HashSet;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use gix::ObjectId;
use log::warn;
use parking_lot::Mutex;

use crate::git;

/// A branch merged into a view: its tip, and its merge point with its parent
/// (none: unrelated histories).
pub struct Member<'a> {
    pub branch: &'a str,
    pub tip: ObjectId,
    pub base: Option<ObjectId>,
}

/// What git says conflicts in a view.
#[derive(Debug, PartialEq)]
pub struct Conflict {
    /// As git says it: `CONFLICT (contents)`, `CONFLICT (modify/delete)`...
    pub kind: String,
    /// The two sides of the merge it is in: the view so far, the member.
    pub sides: [String; 2],
    pub path: Vec<u8>,
}

/// A view's tree, and its conflicts.
pub struct Merged {
    pub tree: ObjectId,
    /// A commit of it in the private repository (`Private::shown`).
    pub commit: ObjectId,
    pub conflicts: Vec<Conflict>,
}

/// The private repository, and where git runs for the merges.
pub struct Private {
    dir: PathBuf,
    /// The repository's git dir, and its objects.
    repository: PathBuf,
    objects: PathBuf,
    /// Held by each merge and its garbage collection: one would prune what
    /// the other just wrote.
    lock: Mutex<()>,
}

impl Private {
    /// The private repository of the repository at `common_dir`, made anew
    /// (an earlier mount's objects dropped) and opened for reads.
    pub fn create(common_dir: &Path) -> Result<Private> {
        let dir = crate::state::state_root(common_dir).join(".views.git");
        crate::state::retire::remove_dir(&dir)?;
        std::fs::create_dir_all(&dir)?;
        let init = ["init", "-q", "--bare"];
        git::cmd::output_of(&mut git::cmd::command_in_other(&dir)?, &init, None)?;
        let objects = common_dir.join("objects").canonicalize()?;
        std::fs::create_dir_all(dir.join("objects/info"))?;
        std::fs::write(
            dir.join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )?;
        let private = Private {
            dir,
            repository: common_dir.to_path_buf(),
            objects,
            lock: Mutex::new(()),
        };
        // Its commits' author, and no reflogs: they would keep what no view
        // shows.
        for (key, value) in [
            ("user.name", "gitfold"),
            ("user.email", "gitfold@localhost"),
            ("core.logAllRefUpdates", "false"),
        ] {
            private.run(&["config", key, value])?;
        }
        git::open_private(&private.dir)?;
        Ok(private)
    }

    /// git in the private repository.
    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        git::cmd::output_of(&mut git::cmd::command_in_other(&self.dir)?, args, None)
    }

    /// git in the repository, as in a shell at its work tree's top (its
    /// settings, attributes, merge drivers), but for its objects: read from
    /// the repository and the private one, written to the private one only.
    /// Missing objects aren't fetched (gitfold fetches what a view needs
    /// first).
    fn in_repository(&self) -> Result<Command> {
        let mut git = match git::work_tree() {
            Some(top) => git::cmd::in_workspace(&top)?,
            None => {
                let mut git = git::cmd::command_in_other(&self.repository)?;
                git.current_dir(&self.repository);
                git
            }
        };
        git.env("GIT_OBJECT_DIRECTORY", self.dir.join("objects"))
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate(&self.objects))
            .env("GIT_NO_LAZY_FETCH", "1");
        Ok(git)
    }

    /// A commit of `tree`, with no parent (nothing of any history to keep),
    /// unsigned whatever the settings.
    fn commit(&self, tree: &ObjectId, message: &str) -> Result<ObjectId> {
        let hex = tree.to_string();
        let out = self.run(&["commit-tree", "--no-gpg-sign", &hex, "-m", message])?;
        Ok(ObjectId::from_hex(
            String::from_utf8_lossy(&out).trim().as_bytes(),
        )?)
    }

    /// Runs `git update-ref --stdin` with `commands`.
    fn refs(&self, commands: String) -> Result<()> {
        let args = ["update-ref", "--stdin"];
        let mut git = git::cmd::command_in_other(&self.dir)?;
        git::cmd::output_of(&mut git, &args, Some(commands.into_bytes())).map(drop)
    }

    /// The view of `branch`: its commit and tree `own` (the tip of `branch`
    /// or of `origin`, its origin root) with `members` merged in turn. Its
    /// commit is kept (`refs/gitfold/next/<branch>`) until `shown` says it
    /// is the one shown.
    pub fn merge(
        &self,
        branch: &str,
        origin: Option<&str>,
        own: (ObjectId, ObjectId),
        members: &[Member],
    ) -> Result<Merged> {
        let _lock = self.lock.lock();
        let local = format!("refs/heads/{branch}");
        let own_refs: Vec<&str> = [local.as_str()].into_iter().chain(origin).collect();
        // git's arguments for the view so far and the member, which name
        // the sides in conflict markers: refs, then the view's tree.
        let mut ours = named(&own_refs, own.0);
        // How the summary names the view so far.
        let mut side = branch.to_string();
        let mut tree = own.1;
        let mut conflicts = Vec::new();
        for (i, m) in members.iter().enumerate() {
            let base = (m.base).unwrap_or_else(|| ObjectId::empty_tree(gix::hash::Kind::Sha1));
            let base = format!("--merge-base={base}");
            let theirs = named(&[&format!("refs/heads/{}", m.branch)], m.tip);
            // Attributes (`merge=union`) from the view so far, as a merge in a
            // checkout of it reads them.
            let attributes = format!("attr.tree={tree}");
            let args = ["merge-tree", "--write-tree", "-z", &base, &ours, &theirs];
            let out = (self.in_repository()?.args(["-c", &attributes]).args(args))
                .stdin(Stdio::null())
                .output()?;
            // 1: conflicts, which the output says.
            if !matches!(out.status.code(), Some(0 | 1)) {
                return Err(git::cmd::Failed {
                    args: args.join(" "),
                    status: out.status,
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                }
                .into());
            }
            let found;
            (tree, found) = parse(&out.stdout)?;
            for (kind, path) in found {
                let sides = [side.clone(), m.branch.to_string()];
                conflicts.push(Conflict { kind, sides, path });
            }
            side = label(branch, &side, m.branch, i + 1);
            ours = tree.to_string();
        }
        // Kept as a tree of its own, without the branches' history.
        let commit = self.commit(&tree, branch)?;
        self.refs(format!("update refs/gitfold/next/{branch} {commit}\n"))?;
        Ok(Merged {
            tree,
            commit,
            conflicts,
        })
    }

    /// The view of `branch` now shows `commit` (from `merge`): it is kept
    /// instead of the one before, and git garbage-collects as it decides
    /// (`gc --auto`, with `gc.auto`'s default whatever the settings say,
    /// pruning at once): what no view shows goes. A failure of that is only
    /// logged.
    pub fn shown(&self, branch: &str, commit: ObjectId) -> Result<()> {
        let _lock = self.lock.lock();
        self.refs(format!(
            "update refs/gitfold/views/{branch} {commit}\ndelete refs/gitfold/next/{branch}\n"
        ))?;
        let packs = self.dir.join("objects/pack");
        let before = std::fs::metadata(&packs).and_then(|m| m.modified()).ok();
        let gc = self.run(&[
            "-c",
            "gc.auto=6700",
            "-c",
            "gc.autoDetach=false",
            "-c",
            "gc.pruneExpire=now",
            "gc",
            "--auto",
            "--quiet",
        ]);
        if let Err(e) = gc {
            warn!("views: garbage-collecting their private repository: {e:#}");
        }
        // Packs it made.
        if std::fs::metadata(&packs).and_then(|m| m.modified()).ok() != before {
            git::refresh_private();
        }
        Ok(())
    }
}

/// How git is given commit `id`, which the first of `refs` (full refs) that
/// still names it names: by the shortest name git takes for that ref (as
/// `%(refname:short)`), for conflict markers; by its id if none does (one
/// moved since, the view is made again).
fn named(refs: &[&str], id: ObjectId) -> String {
    let at = |name: &str| git::resolve(name).is_ok_and(|c| c.id == id);
    match refs.iter().find(|full| at(full)) {
        Some(full) => {
            let short = crate::tree::short(full);
            match git::full_ref(short).as_deref() == Some(*full) {
                true => short.to_string(),
                false => full.to_string(),
            }
        }
        None => id.to_string(),
    }
}

/// `dir` as an entry of `GIT_ALTERNATE_OBJECT_DIRECTORIES` (`:`-separated):
/// quoted, C-style, if it would be taken apart.
fn alternate(dir: &Path) -> OsString {
    let text = dir.as_os_str().as_bytes();
    if !text.contains(&b':') && !text.starts_with(b"\"") {
        return dir.as_os_str().to_owned();
    }
    let mut quoted = b"\"".to_vec();
    for &b in text {
        if b == b'"' || b == b'\\' {
            quoted.push(b'\\');
        }
        quoted.push(b);
    }
    quoted.push(b'"');
    OsString::from_vec(quoted)
}

/// How the summary names the view so far after `member`, the `merged`th
/// member, was merged into `ours` (a view of `branch`): a long name counts
/// the branches.
fn label(branch: &str, ours: &str, member: &str, merged: usize) -> String {
    let label = format!("{ours}+{member}");
    match label.len() > 100 {
        true => format!("{branch}+{merged}-branches"),
        false => label,
    }
}

/// What git says conflicts in a merge: its kind, the path.
type Found = Vec<(String, Vec<u8>)>;

/// The tree and the conflicts (`CONFLICT (…)` messages: kind, path) of a
/// `git merge-tree --write-tree -z` output: the tree, the conflicted files'
/// stages (a mode, an id and a stage, a tab, a path), an empty field, then
/// each message as its number of paths, the paths, its type, its text.
fn parse(out: &[u8]) -> Result<(ObjectId, Found)> {
    let mut fields = out.split(|b| *b == 0);
    let tree = fields.next().context("no tree in merge-tree's output")?;
    let tree = ObjectId::from_hex(tree)?;
    let mut fields = fields.peekable();
    // The conflicted files' stages, then an empty field.
    while fields.peek().is_some_and(|f| f.contains(&b'\t')) {
        fields.next();
    }
    if fields.peek().is_some_and(|f| f.is_empty()) {
        fields.next();
    }
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    while let Some(count) = fields.next().filter(|f| !f.is_empty()) {
        let count: usize = std::str::from_utf8(count)?.parse()?;
        let paths: Vec<&[u8]> = fields.by_ref().take(count).collect();
        let kind = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
        fields.next(); // the text
        if kind.starts_with("CONFLICT") {
            for path in paths {
                if seen.insert((kind.clone(), path)) {
                    found.push((kind.clone(), path.to_vec()));
                }
            }
        }
    }
    Ok((tree, found))
}

#[cfg(test)]
mod tests {
    use super::super::test_repo::{branch, commit, private, run};
    use super::*;

    fn id(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    /// The view of `n` with `members` (branch, parent) merged in, by git.
    fn view(n: &str, members: &[(&str, &str)]) -> Merged {
        let inputs: Vec<Member> = (members.iter())
            .map(|(b, p)| {
                let (b_ref, p_ref) = (format!("refs/heads/{b}"), format!("refs/heads/{p}"));
                Member {
                    branch: b,
                    tip: id(&run(&["rev-parse", &b_ref])),
                    base: Some(id(&run(&["merge-base", &b_ref, &p_ref]))),
                }
            })
            .collect();
        private()
            .merge(
                n,
                None,
                (
                    id(&run(&["rev-parse", n])),
                    id(&run(&["rev-parse", &format!("{n}^{{tree}}")])),
                ),
                &inputs,
            )
            .unwrap()
    }

    /// What the merged tree has at `path`, read as gitfold reads it.
    fn show(merged: &Merged, path: &str) -> Option<String> {
        let store = crate::store::Store::default();
        let tree = store.load_tree(merged.tree).unwrap();
        let crate::store::Lower::File(id) = store.entry_at(tree, path.as_bytes())? else {
            return None;
        };
        let data = git::read_blob(store.file(id).unwrap().oid).unwrap();
        Some(String::from_utf8(data).unwrap())
    }

    #[test]
    fn siblings_and_conflicts_named_by_branch() {
        let api = branch(
            "g1-api",
            &commit(None, &[("a", Some("1\n2\n3\n4\n5\n")), ("c", Some("c\n"))]),
        );
        branch(
            "g1-ui",
            &commit(
                Some(&api),
                &[("a", Some("UI\n2\n3\n4\n5\n")), ("c", Some("ui\n"))],
            ),
        );
        branch(
            "g1-web",
            &commit(
                Some(&api),
                &[("a", Some("1\n2\n3\n4\nWEB\n")), ("c", Some("web\n"))],
            ),
        );
        let m = view("g1-api", &[("g1-ui", "g1-api"), ("g1-web", "g1-api")]);
        assert_eq!(show(&m, "a").unwrap(), "UI\n2\n3\n4\nWEB\n");
        // The view so far named by its tree, the member by name.
        let c = show(&m, "c").unwrap();
        let (ours, rest) = c.split_once('\n').unwrap();
        let tree = ours.strip_prefix("<<<<<<< ").unwrap();
        assert!(ObjectId::from_hex(tree.as_bytes()).is_ok(), "{c}");
        assert_eq!(rest, "ui\n=======\nweb\n>>>>>>> g1-web\n");
        let sides = ["g1-api+g1-ui".to_string(), "g1-web".to_string()];
        assert_eq!(
            m.conflicts,
            [Conflict {
                kind: "CONFLICT (contents)".into(),
                sides,
                path: b"c".to_vec()
            }]
        );
    }

    #[test]
    fn each_brings_only_its_own_work() {
        // ui left api at api0, api moved on; css left ui, ui moved on.
        let api0 = commit(None, &[("a", Some("1\n2\n3\n")), ("u", Some("u\n"))]);
        let ui0 = commit(Some(&api0), &[("a", Some("1\n2\nUI\n"))]);
        branch("g2-css", &commit(Some(&ui0), &[("c", Some("css\n"))]));
        branch("g2-ui", &commit(Some(&ui0), &[("u", Some("u2\n"))]));
        branch(
            "g2-api",
            &commit(Some(&api0), &[("a", Some("API\n2\n3\n"))]),
        );
        let m = view("g2-api", &[("g2-ui", "g2-api"), ("g2-css", "g2-ui")]);
        assert_eq!(show(&m, "a").unwrap(), "API\n2\nUI\n");
        assert_eq!(
            show(&m, "u").unwrap(),
            "u2\n",
            "css's old copy of u isn't its change"
        );
        assert_eq!(show(&m, "c").unwrap(), "css\n");
        assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
    }

    #[test]
    fn renames_and_attributes_as_git_merges() {
        let files = [
            ("old", Some("x\n")),
            ("CHANGES", Some("c\n")),
            (".gitattributes", Some("CHANGES merge=union\n")),
        ];
        let api = branch("g3-api", &commit(None, &files));
        branch(
            "g3-ui",
            &commit(
                Some(&api),
                &[
                    ("old", None),
                    ("new", Some("x\n")),
                    ("CHANGES", Some("c\nui\n")),
                ],
            ),
        );
        branch(
            "g3-web",
            &commit(
                Some(&api),
                &[("old", Some("x\ny\n")), ("CHANGES", Some("c\nweb\n"))],
            ),
        );
        let m = view("g3-api", &[("g3-ui", "g3-api"), ("g3-web", "g3-api")]);
        assert_eq!(
            show(&m, "new").unwrap(),
            "x\ny\n",
            "web's edit followed the rename"
        );
        assert_eq!(show(&m, "old"), None);
        assert_eq!(show(&m, "CHANGES").unwrap(), "c\nui\nweb\n", "merge=union");
        assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
    }

    #[test]
    fn nothing_in_the_repository() {
        let api = branch("g4-api", &commit(None, &[("a", Some("1\n"))]));
        branch("g4-ui", &commit(Some(&api), &[("a", Some("ui\n"))]));
        branch("g4-web", &commit(Some(&api), &[("a", Some("web\n"))]));
        let m = view("g4-api", &[("g4-ui", "g4-api"), ("g4-web", "g4-api")]);
        let tree = m.tree.to_string();
        // Its own objects only: no alternates there.
        let there = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(super::super::test_repo::git_dir())
            .args(["cat-file", "-e", &tree])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(
            !there.success(),
            "the merged tree is in the private repository only"
        );
        assert!(show(&m, "a").unwrap().contains(">>>>>>> g4-web"));
        let refs = run(&["for-each-ref", "--format=%(refname)"]);
        assert!(!refs.contains("gitfold"), "{refs}");
    }

    #[test]
    fn first_fold_names_branches_as_git_takes_them() {
        let api0 = commit(None, &[("a", Some("1\n"))]);
        branch("g5-ui", &commit(Some(&api0), &[("a", Some("ui\n"))]));
        branch("g5-api", &commit(Some(&api0), &[("a", Some("api\n"))]));
        let m = view("g5-api", &[("g5-ui", "g5-api")]);
        let markers = "<<<<<<< g5-api\napi\n=======\nui\n>>>>>>> g5-ui\n";
        assert_eq!(show(&m, "a").unwrap(), markers);
        // A tag of that name: git would take the name for the tag.
        run(&["tag", "g5-ui", &api0]);
        let m = view("g5-api", &[("g5-ui", "g5-api")]);
        let markers = "<<<<<<< g5-api\napi\n=======\nui\n>>>>>>> refs/heads/g5-ui\n";
        assert_eq!(show(&m, "a").unwrap(), markers);
    }

    #[test]
    fn labels() {
        assert_eq!(label("api", "api", "web", 1), "api+web");
        let long = label("api", "api+ui", &"x".repeat(120), 3);
        assert_eq!(long, "api+3-branches");
    }

    #[test]
    fn alternates_quoted() {
        assert_eq!(alternate(Path::new("/a/b")), "/a/b");
        assert_eq!(alternate(Path::new("/a:b/\"c\\")), "\"/a:b/\\\"c\\\\\"");
    }

    #[test]
    fn parses_merge_tree_output() {
        let hex = "a4b67312271adb9d0b19768cb260f1738a040100";
        let out = format!(
            "{hex}\x00100644 6178 1\td/b\x00100644 eedd 2\td/b\x00\x00\
             1\x00a\x00Auto-merging\x00Auto-merging a\n\x00\
             1\x00d/b\x00CONFLICT (contents)\x00CONFLICT (content): Merge conflict in d/b\n\x00\
             2\x00x~web\x00x\x00CONFLICT (file/directory)\x00moved\n\x00"
        );
        let (tree, found) = parse(out.as_bytes()).unwrap();
        assert_eq!(tree.to_string(), hex);
        let found: Vec<(&str, &[u8])> = found.iter().map(|(k, p)| (k.as_str(), &p[..])).collect();
        assert_eq!(
            found,
            [
                ("CONFLICT (contents)", &b"d/b"[..]),
                ("CONFLICT (file/directory)", b"x~web"),
                ("CONFLICT (file/directory)", b"x"),
            ]
        );
        let clean = format!("{hex}\x00");
        assert_eq!(parse(clean.as_bytes()).unwrap().1, []);
    }
}
