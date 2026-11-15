//! How git sees a mixed file: a clean/smudge filter that gitfold answers.
//!
//! A union file (`merge=union`) shows the workspace's own lines among other
//! layers' lines. git must see only the workspace's own, with HEAD on its
//! branch. So each such path gets the attribute `filter=gitfold-<id>`, in the
//! worktree's own attributes file, and the driver is defined only in the
//! worktree's own config, with `required = true` (`worktree_config::set_filter`):
//! - **clean** (git reads the file into an object: `add`, `status`, `diff`,
//!   `commit -a`, `stash`) strips the layers' lines;
//! - **smudge** (git writes an object out: `checkout`, `switch`, `reset`,
//!   `stash pop`, `merge`) puts them back.
//!
//! The index, commits and stashes hold only the workspace's lines; the file
//! shows everyone's. Both are a function of the file's two versions the view
//! has (`Versions`): the base's (below the layers) and the union (with the
//! layers' lines). Clean reverts the change from the base's version to the
//! union in the content it gets (a 3-way merge); where that conflicts (the
//! workspace's lines sit next to the layers'), the layers' added lines are
//! taken out, once each. Smudge applies the same change (a 3-way merge,
//! both kept where both sides only add at one spot, the layers' lines first,
//! as a switch places the workspace's own after them); where it can't, the
//! content is left as it is, without the layers' lines.
//!
//! git talks to the driver with its long-running filter protocol
//! (`filter.<driver>.process`, gitprotocol-long-running-process(5)): one
//! process per git command, started only if a filtered path is converted.
//! That process (`gitfold filter-process`, `serve_git`) relays each request
//! to the daemon over `<common>/gitfold/.filter` (`Frame`), which answers it
//! from the workspace's view as it is now (`fs::workspace::layers`).

use std::io::{BufRead, Read, Write};

use anyhow::{Context, Result, bail};

use super::layers::{added_lines, is_binary, keep_conflicts, merge3, only_adds, without_lines};

/// The two versions of a union file a filter request is answered from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Versions {
    /// The base's version, below the layers.
    pub base: Vec<u8>,
    /// The union: the base's version with the layers' lines.
    pub union: Vec<u8>,
}

impl Versions {
    /// `work` (the file as it is) without the layers' lines.
    pub fn clean(&self, work: &[u8]) -> Vec<u8> {
        let Versions { base, union } = self;
        if work == union.as_slice() {
            return base.clone();
        }
        if base == union || [base, union].iter().any(|d| is_binary(d)) || is_binary(work) {
            return work.to_vec();
        }
        if let Some(clean) = merge3(union, work, base, keep_conflicts()) {
            return clean;
        }
        without_lines(work, &added_lines(base, union))
    }

    /// `own` (what git has) with the layers' lines.
    pub fn smudge(&self, own: &[u8]) -> Vec<u8> {
        let Versions { base, union } = self;
        if own == base.as_slice() {
            return union.clone();
        }
        if base == union || [base, union].iter().any(|d| is_binary(d)) || is_binary(own) {
            return own.to_vec();
        }
        if let Some(merged) = merge3(base, union, own, keep_conflicts()) {
            return merged;
        }
        if only_adds(base, own) && only_adds(base, union) {
            let union_style = gix::merge::blob::builtin_driver::text::Conflict::ResolveWithUnion;
            if let Some(merged) = merge3(base, union, own, union_style) {
                return merged;
            }
        }
        own.to_vec()
    }

    /// The lines of the union the layers added (each without its newline).
    pub fn layers_lines(&self) -> Vec<&[u8]> {
        added_lines(&self.base, &self.union)
            .into_iter()
            .map(super::layers::key)
            .collect()
    }
}

/// What git asks of the filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Clean,
    Smudge,
}

impl Op {
    fn byte(self) -> u8 {
        match self {
            Op::Clean => b'c',
            Op::Smudge => b's',
        }
    }

    fn of(byte: u8) -> Result<Op> {
        match byte {
            b'c' => Ok(Op::Clean),
            b's' => Ok(Op::Smudge),
            _ => bail!("not a filter operation: {byte}"),
        }
    }
}

// The relay's frames to the daemon, after a first line naming the workspace
// (`gitfold-filter 1\n<id>\n`): a request is the operation's byte, the
// path's length (4 bytes, big-endian), the path, the content's length (8
// bytes) and the content; an answer is 0 (the content follows) or 1 (an
// error message follows), the length (8 bytes) and the bytes.

/// The line that opens a relay's connection to the daemon.
pub const HELLO: &str = "gitfold-filter 1";

/// Writes a request for `path`'s `content`.
pub fn write_request(w: &mut impl Write, op: Op, path: &[u8], content: &[u8]) -> Result<()> {
    w.write_all(&[op.byte()])?;
    w.write_all(&u32::try_from(path.len())?.to_be_bytes())?;
    w.write_all(path)?;
    w.write_all(&(content.len() as u64).to_be_bytes())?;
    w.write_all(content)?;
    Ok(w.flush()?)
}

/// A request: what, for which path, of what content.
pub type Request = (Op, Vec<u8>, Vec<u8>);

/// Reads a request: `None` at the end of the stream.
pub fn read_request(r: &mut impl Read) -> Result<Option<Request>> {
    let mut op = [0u8];
    match r.read_exact(&mut op) {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        done => done?,
    }
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut path = vec![0; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut path)?;
    let content = read_sized(r)?;
    Ok(Some((Op::of(op[0])?, path, content)))
}

/// Writes an answer: the content, or why there is none.
pub fn write_answer(w: &mut impl Write, answer: &Result<Vec<u8>>) -> Result<()> {
    let (tag, bytes) = match answer {
        Ok(content) => (0u8, content.clone()),
        Err(e) => (1u8, format!("{e:#}").into_bytes()),
    };
    w.write_all(&[tag])?;
    w.write_all(&(bytes.len() as u64).to_be_bytes())?;
    w.write_all(&bytes)?;
    Ok(w.flush()?)
}

/// Reads an answer.
pub fn read_answer(r: &mut impl Read) -> Result<Result<Vec<u8>>> {
    let mut tag = [0u8];
    r.read_exact(&mut tag)?;
    let bytes = read_sized(r)?;
    Ok(match tag[0] {
        0 => Ok(bytes),
        _ => Err(anyhow::anyhow!("{}", String::from_utf8_lossy(&bytes))),
    })
}

fn read_sized(r: &mut impl Read) -> Result<Vec<u8>> {
    let mut len = [0u8; 8];
    r.read_exact(&mut len)?;
    let len = u64::from_be_bytes(len);
    let mut out = Vec::with_capacity(len.min(1 << 24) as usize);
    r.take(len).read_to_end(&mut out)?;
    anyhow::ensure!(out.len() as u64 == len, "the stream ended early");
    Ok(out)
}

// git's pkt-line format (gitprotocol-common(5)): four hex digits giving the
// length, those included, then the data; `0000` is a flush.

/// The most data in one pkt-line.
const PKT_MAX: usize = 65516;

/// A pkt-line: its data, or `None` for a flush.
fn read_pkt(r: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = usize::from_str_radix(std::str::from_utf8(&len)?, 16).context("a pkt-line length")?;
    if len == 0 {
        return Ok(None);
    }
    anyhow::ensure!(len > 4, "a bad pkt-line length {len}");
    let mut data = vec![0; len - 4];
    r.read_exact(&mut data)?;
    Ok(Some(data))
}

/// A pkt-line of text: without its newline.
fn read_text(r: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    Ok(read_pkt(r)?.map(|mut line| {
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        line
    }))
}

/// The text pkt-lines up to a flush.
fn read_list(r: &mut impl BufRead) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    while let Some(line) = read_text(r)? {
        out.push(line);
    }
    Ok(out)
}

fn write_pkt(w: &mut impl Write, data: &[u8]) -> Result<()> {
    write!(w, "{:04x}", data.len() + 4)?;
    Ok(w.write_all(data)?)
}

fn write_text(w: &mut impl Write, line: &str) -> Result<()> {
    write_pkt(w, format!("{line}\n").as_bytes())
}

fn flush(w: &mut impl Write) -> Result<()> {
    w.write_all(b"0000")?;
    Ok(w.flush()?)
}

/// Serves git's long-running filter protocol on `input` and `output` (the
/// filter process's stdin and stdout) until git closes it, answering each
/// clean or smudge with `answer(op, path, content)`. An answer's error is
/// reported to git as `status=error` (git then fails the command, the
/// filter being required) and on stderr.
pub fn serve_git(
    input: &mut impl BufRead,
    output: &mut impl Write,
    mut answer: impl FnMut(Op, &[u8], Vec<u8>) -> Result<Vec<u8>>,
) -> Result<()> {
    let hello = read_list(input)?;
    anyhow::ensure!(
        hello.first().map(Vec::as_slice) == Some(b"git-filter-client")
            && hello.iter().any(|l| l == b"version=2"),
        "not git's filter protocol, version 2"
    );
    write_text(output, "git-filter-server")?;
    write_text(output, "version=2")?;
    flush(output)?;
    let offered = read_list(input)?;
    for cap in ["capability=clean", "capability=smudge"] {
        if offered.iter().any(|l| l == cap.as_bytes()) {
            write_text(output, cap)?;
        }
    }
    flush(output)?;
    loop {
        let keys = match read_pkt(input) {
            Ok(Some(first)) => {
                let mut keys = vec![first.strip_suffix(b"\n").unwrap_or(&first).to_vec()];
                keys.extend(read_list(input)?);
                keys
            }
            Ok(None) => continue,
            // git closed the pipe: done.
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::UnexpectedEof) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let value = |key: &[u8]| {
            keys.iter()
                .find_map(|l| l.strip_prefix(key).map(<[u8]>::to_vec))
        };
        let op = match value(b"command=").as_deref() {
            Some(b"clean") => Op::Clean,
            Some(b"smudge") => Op::Smudge,
            other => bail!(
                "git asked for {:?}, not clean or smudge",
                other.map(String::from_utf8_lossy)
            ),
        };
        let path = value(b"pathname=").unwrap_or_default();
        let mut content = Vec::new();
        while let Some(pkt) = read_pkt(input)? {
            content.extend_from_slice(&pkt);
        }
        match answer(op, &path, content) {
            Ok(out) => {
                write_text(output, "status=success")?;
                flush(output)?;
                for chunk in out.chunks(PKT_MAX) {
                    write_pkt(output, chunk)?;
                }
                flush(output)?;
                // The status stays as it was.
                flush(output)?;
            }
            Err(e) => {
                eprintln!(
                    "gitfold: {}: {e:#}",
                    String::from_utf8_lossy(&path).trim_end()
                );
                write_text(output, "status=error")?;
                flush(output)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(base: &str, union: &str) -> Versions {
        Versions {
            base: base.into(),
            union: union.into(),
        }
    }

    /// Clean undoes smudge, and both give the view's versions exactly.
    #[test]
    fn round_trips() {
        let reg = v("one\ntwo\n", "one\ntwo\nfromA\nfromB\n");
        assert_eq!(reg.clean(b"one\ntwo\nfromA\nfromB\n"), b"one\ntwo\n");
        assert_eq!(reg.smudge(b"one\ntwo\n"), b"one\ntwo\nfromA\nfromB\n");
        for own in [
            "one\ntwo\nmine\n",
            "zero\none\ntwo\n",
            "one\nmid\ntwo\nmine\n",
            "one\n",
            "one\ntwo\nm1\nm2\nm3\n",
        ] {
            let smudged = reg.smudge(own.as_bytes());
            assert_eq!(
                String::from_utf8_lossy(&reg.clean(&smudged)),
                own,
                "smudged: {}",
                String::from_utf8_lossy(&smudged)
            );
        }
        // Own lines appended after the layers' (as a switch carries them).
        assert_eq!(
            reg.smudge(b"one\ntwo\nmine\n"),
            b"one\ntwo\nfromA\nfromB\nmine\n"
        );
        assert_eq!(
            reg.clean(b"one\ntwo\nfromA\nfromB\nmine\n"),
            b"one\ntwo\nmine\n"
        );
        // A line of mine among the layers'.
        assert_eq!(
            reg.clean(b"one\ntwo\nfromA\nmine\nfromB\n"),
            b"one\ntwo\nmine\n"
        );
        // Nothing of the layers': unchanged.
        let other = b"other\ncontent\n";
        assert_eq!(reg.smudge(other), other);
        assert_eq!(reg.clean(other), other);
        assert_eq!(reg.layers_lines(), [b"fromA".as_slice(), b"fromB"]);
    }

    /// A layer's line in the middle of the base's lines.
    #[test]
    fn lines_inside() {
        let reg = v("a\nb\nc\nd\n", "a\nb\nfromA\nc\nd\n");
        assert_eq!(
            reg.smudge(b"a\nb\nc\nd\nmine\n"),
            b"a\nb\nfromA\nc\nd\nmine\n"
        );
        assert_eq!(
            reg.clean(b"a\nmine\nb\nfromA\nc\nd\n"),
            b"a\nmine\nb\nc\nd\n"
        );
        assert_eq!(
            reg.clean(b"a\nb\nfromA\nmine\nc\nd\n"),
            b"a\nb\nmine\nc\nd\n"
        );
    }

    fn pkt(data: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_pkt(&mut out, data.as_bytes()).unwrap();
        out
    }

    /// A session as git runs it: the handshake, a clean, a smudge, an error.
    #[test]
    fn git_protocol() {
        let mut input = Vec::new();
        for l in ["git-filter-client\n", "version=2\n"] {
            input.extend(pkt(l));
        }
        input.extend(b"0000");
        for l in [
            "capability=clean\n",
            "capability=smudge\n",
            "capability=delay\n",
        ] {
            input.extend(pkt(l));
        }
        input.extend(b"0000");
        for (cmd, path, content) in [
            ("clean", "reg.txt", "one\nA\n"),
            ("smudge", "reg.txt", "one\n"),
            ("clean", "bad.txt", "x\n"),
        ] {
            input.extend(pkt(&format!("command={cmd}\n")));
            input.extend(pkt(&format!("pathname={path}\n")));
            input.extend(b"0000");
            input.extend(pkt(content));
            input.extend(b"0000");
        }
        let mut output = Vec::new();
        let mut seen = Vec::new();
        serve_git(&mut input.as_slice(), &mut output, |op, path, content| {
            seen.push((op, path.to_vec()));
            if path == b"bad.txt" {
                bail!("no");
            }
            Ok(match op {
                Op::Clean => content.strip_suffix(b"A\n").unwrap().to_vec(),
                Op::Smudge => [content, b"A\n".to_vec()].concat(),
            })
        })
        .unwrap();
        let expected = [
            pkt("git-filter-server\n"),
            pkt("version=2\n"),
            b"0000".to_vec(),
            pkt("capability=clean\n"),
            pkt("capability=smudge\n"),
            b"0000".to_vec(),
            pkt("status=success\n"),
            b"0000".to_vec(),
            pkt("one\n"),
            b"00000000".to_vec(),
            pkt("status=success\n"),
            b"0000".to_vec(),
            pkt("one\nA\n"),
            b"00000000".to_vec(),
            pkt("status=error\n"),
            b"0000".to_vec(),
        ]
        .concat();
        assert_eq!(
            String::from_utf8_lossy(&output),
            String::from_utf8_lossy(&expected)
        );
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1], (Op::Smudge, b"reg.txt".to_vec()));
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_request(&mut buf, Op::Smudge, b"a/b.txt", b"content").unwrap();
        let (op, path, content) = read_request(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(
            (op, &path[..], &content[..]),
            (Op::Smudge, &b"a/b.txt"[..], &b"content"[..])
        );
        assert!(read_request(&mut &b""[..]).unwrap().is_none());
        let mut buf = Vec::new();
        write_answer(&mut buf, &Ok(b"x".to_vec())).unwrap();
        write_answer(&mut buf, &Err(anyhow::anyhow!("why"))).unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_answer(&mut r).unwrap().unwrap(), b"x");
        assert_eq!(read_answer(&mut r).unwrap().unwrap_err().to_string(), "why");
    }
}
