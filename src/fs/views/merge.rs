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
//! too (`git::open_private`). No object or ref is written into the repository
//! (merge drivers run in its top, as in any merge). git is given commits by
//! id, so conflict markers name the sides by id; the summary names the
//! branches. The private repository's only refs are the view's: the tree it
//! shows (`refs/gitfold/views/<view>`) and, until it shows it, the new one
//! (`refs/gitfold/next/<view>`). It is made anew at each mount, and
//! garbage-collected as git's `gc --auto` decides (more loose objects than
//! `gc.auto`'s default): what no view shows goes. Missing objects aren't
//! fetched: in a partial clone, gitfold fetches what a merge needs first, as
//! for a snapshot.

use std::collections::HashSet;
use std::os::unix::ffi::OsStrExt;
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
    pub(super) dir: PathBuf,
    /// The repository's git dir, its work tree's top (git's current
    /// directory there; its git dir if it has none).
    git_dir: PathBuf,
    top: PathBuf,
    /// The repository's object format, and so the private one's.
    hash: gix::hash::Kind,
    /// Held by each merge, each live tree (`live`) and each garbage
    /// collection: one would prune what another just wrote.
    pub(super) lock: Mutex<super::live::Told>,
}

impl Private {
    /// The private repository of the repository at `common_dir` (the one
    /// `git::init` opened), made anew (an earlier mount's objects dropped)
    /// and opened for reads.
    pub fn create(common_dir: &Path) -> Result<Private> {
        let dir = crate::state::state_root(common_dir).join(".views.git");
        crate::state::retire::remove_dir(&dir)?;
        std::fs::create_dir_all(&dir)?;
        let objects = common_dir.join("objects").canonicalize()?;
        let (git_dir, hash) =
            git::with_repo(|repo| (repo.git_dir().to_path_buf(), repo.object_hash()));
        let private = Private {
            dir,
            top: git::work_tree().unwrap_or_else(|| git_dir.clone()),
            git_dir,
            hash,
            lock: Mutex::default(),
        };
        // In the repository's object format: git writes its objects there.
        private.run(&["init", "-q", "--bare", &format!("--object-format={hash}")])?;
        std::fs::create_dir_all(private.dir.join("objects/info"))?;
        std::fs::write(
            private.dir.join("objects/info/alternates"),
            // Quoted as git quotes paths (one per line).
            [
                crate::cli::quote(objects.as_os_str().as_bytes()),
                b"\n".to_vec(),
            ]
            .concat(),
        )?;
        // Its commits' author, and no reflogs: they would keep what no view
        // shows.
        for (key, value) in [
            ("user.name", "gitfold"),
            ("user.email", "gitfold@localhost"),
            ("core.logAllRefUpdates", "false"),
        ] {
            private.run(&["config", key, value])?;
        }
        // In a partial clone, the repository's promisor remotes: what they
        // promise (blobs the merged trees name) its garbage collection leaves
        // alone, instead of failing on it.
        for remote in git::promisor::remotes() {
            private.run(&["config", &format!("remote.{remote}.promisor"), "true"])?;
        }
        git::open_private(&private.dir)?;
        Ok(private)
    }

    /// git in the private repository: no hook (a global `core.hooksPath`'s
    /// `pre-auto-gc`, `reference-transaction`) hears of it, and nothing is
    /// fetched.
    fn private_git(&self) -> Result<Command> {
        let mut git = git::cmd::command_in_other(&self.dir)?;
        git.args(["-c", "core.hooksPath=/dev/null"])
            .env("GIT_NO_LAZY_FETCH", "1");
        Ok(git)
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        git::cmd::output_of(&mut self.private_git()?, args, None)
    }

    /// git in the repository, as there (its settings, attributes, merge
    /// drivers; in its work tree's top), but for its objects: read from the
    /// repository and the private one, written to the private one only.
    /// Missing objects aren't fetched (gitfold fetches what a view needs
    /// first).
    fn in_repository(&self) -> Result<Command> {
        let mut git = git::cmd::command_in_other(&self.git_dir)?;
        git.current_dir(&self.top);
        self.to_private(&mut git);
        Ok(git)
    }

    /// `git`, its objects read from the repository and the private one and
    /// written to the private one only, missing ones not fetched.
    pub(super) fn to_private<'a>(&self, git: &'a mut Command) -> &'a mut Command {
        git.env("GIT_OBJECT_DIRECTORY", self.dir.join("objects"))
            .env("GIT_NO_LAZY_FETCH", "1")
    }

    /// A commit of `tree`, with no parent (nothing of any history to keep),
    /// unsigned whatever the settings.
    pub(super) fn commit(&self, tree: &ObjectId, message: &str) -> Result<ObjectId> {
        let hex = tree.to_string();
        oid(&self.run(&["commit-tree", "--no-gpg-sign", &hex, "-m", message])?)
    }

    /// Runs `git update-ref --stdin` with `commands`.
    pub(super) fn refs(&self, commands: String) -> Result<()> {
        let args = ["update-ref", "--stdin"];
        let input = Some(commands.into_bytes());
        git::cmd::output_of(&mut self.private_git()?, &args, input).map(drop)
    }

    /// The view of `branch`: its commit and tree `own` (the tip of `branch`
    /// or of its origin root) with `members` merged in turn. git is given
    /// commits by id, so conflict markers name the sides by id; the summary
    /// names them by branch. Its commit is kept (`refs/gitfold/next/<branch>`)
    /// until `shown` says it is the one shown.
    pub fn merge(
        &self,
        branch: &str,
        own: (ObjectId, ObjectId),
        members: &[Member],
    ) -> Result<Merged> {
        let _lock = self.lock.lock();
        // The view so far: its commit (a commit of each fold's tree: git
        // 2.44 takes no tree there), and how the summary names it.
        let (mut ours, mut side) = (own.0, branch.to_string());
        let mut tree = own.1;
        let mut conflicts = Vec::new();
        for (i, m) in members.iter().enumerate() {
            let base = (m.base).unwrap_or_else(|| ObjectId::empty_tree(self.hash));
            let base = format!("--merge-base={base}");
            let (ours_hex, theirs) = (ours.to_string(), m.tip.to_string());
            // Attributes (`merge=union`) from the view so far, as a merge in a
            // checkout of it reads them.
            let attributes = format!("attr.tree={tree}");
            let args = [
                "merge-tree",
                "--write-tree",
                "-z",
                &base,
                &ours_hex,
                &theirs,
            ];
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
            side = label(branch, &side, m.branch, i + 1, members);
            ours = self.commit(&tree, &side)?;
        }
        // Kept as a tree of its own, without the branches' history.
        let commit = match members.is_empty() {
            true => self.commit(&tree, branch)?,
            false => ours,
        };
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

/// The object id git printed (and a newline).
pub(super) fn oid(out: &[u8]) -> Result<ObjectId> {
    Ok(ObjectId::from_hex(
        String::from_utf8_lossy(out).trim().as_bytes(),
    )?)
}

/// How the summary names the view so far after `member`, the `merged`th
/// member, was merged into `ours` (a view of `branch`): a long name counts
/// the branches; one that is a member's name too gets `-view`, so that the
/// two sides read apart.
fn label(branch: &str, ours: &str, member: &str, merged: usize, members: &[Member]) -> String {
    let mut label = format!("{ours}+{member}");
    if label.len() > 100 {
        label = format!("{branch}+{merged}-branches");
    }
    while label == branch || members.iter().any(|m| m.branch == label) {
        label.push_str("-view");
    }
    label
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
        let web = run(&["rev-parse", "refs/heads/g1-web"]);
        let c = show(&m, "c").unwrap();
        let (ours, rest) = c.split_once('\n').unwrap();
        let id = ours.strip_prefix("<<<<<<< ").unwrap();
        assert!(ObjectId::from_hex(id.as_bytes()).is_ok(), "{c}");
        assert_eq!(rest, format!("ui\n=======\nweb\n>>>>>>> {web}\n"));
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
        assert!(show(&m, "a").unwrap().contains("=======\nweb\n>>>>>>> "));
        let refs = run(&["for-each-ref", "--format=%(refname)"]);
        assert!(!refs.contains("gitfold"), "{refs}");
    }

    #[test]
    fn markers_by_id_summary_by_branch() {
        let api0 = commit(None, &[("a", Some("1\n"))]);
        let ui = branch("g5-ui", &commit(Some(&api0), &[("a", Some("ui\n"))]));
        let api = branch("g5-api", &commit(Some(&api0), &[("a", Some("api\n"))]));
        let m = view("g5-api", &[("g5-ui", "g5-api")]);
        let markers = format!("<<<<<<< {api}\napi\n=======\nui\n>>>>>>> {ui}\n");
        assert_eq!(show(&m, "a").unwrap(), markers);
        let sides = ["g5-api".to_string(), "g5-ui".to_string()];
        assert_eq!(m.conflicts[0].sides, sides);
    }

    #[test]
    fn labels() {
        let tip = ObjectId::empty_tree(gix::hash::Kind::Sha1);
        let member = |branch| Member {
            branch,
            tip,
            base: None,
        };
        let x = "x".repeat(120);
        let members = [member("ui"), member("api+ui"), member(&x)];
        assert_eq!(label("api", "api", "web", 1, &members), "api+web");
        let clash = label("api", "api", "ui", 1, &members);
        assert_eq!(clash, "api+ui-view", "a member's name");
        let long = label("api", "api+ui", &x, 3, &members);
        assert_eq!(long, "api+3-branches");
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
