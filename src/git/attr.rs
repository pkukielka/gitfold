//! Git attributes as git reads them for a commit: its `.gitattributes`
//! files, then `info/attributes`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use gix::bstr::BString;

use super::cmd;

/// Attribute `attr`'s value for each of `paths` in commit `rev` of the
/// repository at `common_dir` (`unspecified`, `set` and `unset` included,
/// as `git check-attr` says them), in one `git check-attr`.
pub fn values(
    common_dir: &Path,
    rev: &str,
    attr: &str,
    paths: &[BString],
) -> Result<HashMap<BString, BString>> {
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p);
        input.push(0);
    }
    let source = format!("--source={rev}");
    let args = ["check-attr", "-z", "--stdin", &source, attr];
    let out = cmd::output_with_input(common_dir, &args, input)?;
    // path NUL attribute NUL value NUL ...
    let fields: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    Ok((fields.chunks(3))
        .filter(|f| f.len() == 3)
        .map(|f| (f[0].into(), f[2].into()))
        .collect())
}
