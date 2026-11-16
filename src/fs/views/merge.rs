//! The views' merge, by git itself: `git merge-tree --write-tree` folds each
//! member into the view so far, against its merge point with its parent.
//! Renames, `.gitattributes` merge settings and `merge.conflictStyle` are
//! git's; conflicts stay in the files as git leaves them, and what git says
//! of them is the view's summary.
//!
//! Everything git writes goes into a private bare repository,
//! `<git dir>/gitfold/.views.git`, which borrows the repository's objects
//! (`objects/info/alternates`) and which gitfold reads too
//! (`git::open_private`): no object, ref or commit lands in the repository.
//! Its refs name the sides of each merge, so conflict markers read
//! `<<<<<<< api+ui` and `>>>>>>> web`; `refs/gitfold/views/<view>` keeps each
//! view's tree. It is made anew at each mount, and garbage-collected when it
//! holds more loose objects than git's own `gc.auto` (6,700): what the views
//! no longer show goes.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use log::info;
use parking_lot::Mutex;

use crate::git;

/// Git's `gc.auto`: more loose objects than this, and the private repository
/// is garbage-collected.
const LOOSE_LIMIT: usize = 6700;
/// How many merges between two counts of its loose objects.
const COUNT_EVERY: usize = 100;

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
    pub conflicts: Vec<Conflict>,
}

/// The private repository.
pub struct Private {
    dir: PathBuf,
    /// Held by each merge and the garbage collection: they use the same refs.
    lock: Mutex<usize>,
}

impl Private {
    /// The private repository of the repository at `common_dir`, made anew
    /// (an earlier mount's objects dropped) and opened for reads.
    pub fn create(common_dir: &Path) -> Result<Private> {
        let dir = crate::state::state_root(common_dir).join(".views.git");
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", dir.display()));
            }
            _ => {}
        }
        std::fs::create_dir_all(&dir)?;
        let mut init = Command::new("git");
        git::cmd::in_other_worktree(&mut init)?;
        git::cmd::checked(init.args(["init", "-q", "--bare"]).arg(&dir), &["init"])?;
        let objects = common_dir.join("objects").canonicalize()?;
        let alternates = dir.join("objects/info/alternates");
        std::fs::write(&alternates, format!("{}\n", objects.display()))?;
        // The repository's settings (`merge.conflictStyle`, merge drivers),
        // as a merge in it would read them.
        let config = common_dir.join("config").canonicalize()?;
        let mut include = git::cmd::command_in_other(&dir)?;
        let args = ["config", "include.path"];
        git::cmd::checked(include.args(args).arg(&config), &args)?;
        git::open_private(&dir)?;
        Ok(Private {
            dir,
            lock: Mutex::new(0),
        })
    }

    /// git in the private repository: the repository's own settings (its
    /// config is the private one's, which has none), no file system monitor.
    fn git(&self) -> Result<Command> {
        let mut git = git::cmd::command_in_other(&self.dir)?;
        git.args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "user.name=gitfold",
            "-c",
            "user.email=gitfold@localhost",
        ]);
        Ok(git)
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        git::cmd::checked(self.git()?.args(args), args)
    }

    /// Points private ref `name` at `commit`.
    fn point(&self, name: &str, commit: &ObjectId) -> Result<()> {
        self.run(&["update-ref", name, &commit.to_string()])
            .map(drop)
    }

    /// The view `view` (a name for its ref): `own` (the branch and its
    /// commit) with `members` merged in turn.
    pub fn merge(&self, view: &str, own: (&str, ObjectId), members: &[Member]) -> Result<Merged> {
        let mut merges = self.lock.lock();
        let (mut ours, mut commit) = (own.0.to_string(), own.1);
        self.point(&format!("refs/heads/{ours}"), &commit)?;
        let mut tree = None;
        let mut conflicts = Vec::new();
        for m in members {
            self.point(&format!("refs/heads/{}", m.branch), &m.tip)?;
            let base = m
                .base
                .unwrap_or_else(|| ObjectId::empty_tree(gix::hash::Kind::Sha1));
            let base = format!("--merge-base={base}");
            // Attributes (`merge=union`) from the view so far, as a merge in a
            // checkout reads them from it.
            let attributes = format!("attr.tree=refs/heads/{ours}");
            let args = ["merge-tree", "--write-tree", "-z", &base, &ours, m.branch];
            let out = self.git()?.args(["-c", &attributes]).args(args).output()?;
            // 1: conflicts, which the output says.
            if !matches!(out.status.code(), Some(0 | 1)) {
                bail!(
                    "git {} failed ({}): {}",
                    args.join(" "),
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let (merged, found) = parse(&out.stdout)?;
            for (kind, path) in found {
                let sides = [ours.clone(), m.branch.to_string()];
                conflicts.push(Conflict { kind, sides, path });
            }
            ours = format!("{ours}+{}", m.branch);
            let hex = merged.to_string();
            let made = self.run(&["commit-tree", &hex, "-m", &ours])?;
            commit = ObjectId::from_hex(String::from_utf8_lossy(&made).trim().as_bytes())?;
            self.point(&format!("refs/heads/{ours}"), &commit)?;
            tree = Some(merged);
        }
        self.point(&format!("refs/gitfold/views/{view}"), &commit)?;
        let tree = match tree {
            Some(tree) => tree,
            None => git::resolve(&commit.to_string())?.tree,
        };
        *merges += 1;
        if (*merges).is_multiple_of(COUNT_EVERY) {
            self.tidy()?;
        }
        Ok(Merged { tree, conflicts })
    }

    /// Garbage-collects the private repository if it holds more loose
    /// objects than git's `gc.auto`: what no view shows goes (each view's
    /// tree is kept by its ref).
    fn tidy(&self) -> Result<()> {
        let counts = String::from_utf8(self.run(&["count-objects", "-v"])?)?;
        let loose = (counts.lines())
            .find_map(|l| l.strip_prefix("count: "))
            .and_then(|n| n.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if loose > LOOSE_LIMIT {
            info!("views: {loose} loose objects, garbage-collecting them");
            self.run(&["gc", "--quiet", "--prune=now"])?;
        }
        Ok(())
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
    while let Some(count) = fields.next().filter(|f| !f.is_empty()) {
        let count: usize = std::str::from_utf8(count)?.parse()?;
        let paths: Vec<&[u8]> = fields.by_ref().take(count).collect();
        let kind = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
        fields.next(); // the text
        if kind.starts_with("CONFLICT") {
            for path in paths {
                let entry = (kind.clone(), path.to_vec());
                if !found.contains(&entry) {
                    found.push(entry);
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
            .map(|(b, p)| Member {
                branch: b,
                tip: id(&run(&["rev-parse", b])),
                base: Some(id(&run(&["merge-base", b, p]))),
            })
            .collect();
        private()
            .merge(n, (n, id(&run(&["rev-parse", n]))), &inputs)
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
        assert_eq!(
            show(&m, "c").unwrap(),
            "<<<<<<< g1-api+g1-ui\nui\n=======\nweb\n>>>>>>> g1-web\n"
        );
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
        assert!(show(&m, "a").unwrap().contains("<<<<<<< g4-api+g4-ui"));
        let refs = run(&["for-each-ref", "--format=%(refname)"]);
        assert!(
            !refs.contains("g4-api+g4-ui") && !refs.contains("gitfold/views"),
            "{refs}"
        );
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
