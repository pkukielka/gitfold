//! `gitfold bench-attrs`: evaluates checkout-relevant attributes for every file
//! of a revision, to measure what applying checkout filters would cost.

use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::Result;
use gix::attrs::StateRef;
use gix::attrs::search::Outcome;
use gix::worktree::stack::state::attributes::Source;
use rayon::prelude::*;

use crate::git;

const ATTRS: [&str; 5] = ["text", "eol", "filter", "ident", "working-tree-encoding"];

/// Whether checkout would have to convert the file (with Linux defaults).
fn needs_conversion(out: &Outcome) -> bool {
    out.iter_selected()
        .any(|m| match (m.assignment.name.as_str(), m.assignment.state) {
            ("eol", StateRef::Value(v)) => v.as_bstr() == "crlf",
            ("filter" | "working-tree-encoding", state) => state != StateRef::Unspecified,
            ("ident", state) => state == StateRef::Set,
            _ => false,
        })
}

pub fn run(rev: &str) -> Result<()> {
    let tree = git::resolve(rev)?.tree;
    git::with_repo(|repo| {
        let started = Instant::now();
        let index = repo.index_from_tree(&tree)?;
        let indexed = started.elapsed();
        let mut stack = repo.attributes_only(&index, Source::IdMapping)?;
        let mut out = stack.selected_attribute_matches(ATTRS);
        let setup = started.elapsed();
        // Parallel variant: contiguous chunks, one attribute stack per thread.
        let par_started = Instant::now();
        let entries = index.entries();
        let chunk = entries
            .len()
            .div_ceil(rayon::current_num_threads() * 4)
            .max(1);
        let par_convert: usize = entries
            .par_chunks(chunk)
            .map(|part| {
                git::with_repo(|repo| -> Result<usize> {
                    let mut stack = repo.attributes_only(&index, Source::IdMapping)?;
                    let mut out = stack.selected_attribute_matches(ATTRS);
                    let mut n = 0;
                    for e in part {
                        stack
                            .at_entry(e.path(&index), Some(e.mode))?
                            .matching_attributes(&mut out);
                        n += needs_conversion(&out) as usize;
                    }
                    Ok(n)
                })
            })
            .sum::<Result<usize>>()?;
        println!(
            "parallel lookups: {:?} ({par_convert} candidates)",
            par_started.elapsed()
        );
        let seq_started = Instant::now();
        let mut combos: BTreeMap<String, usize> = BTreeMap::new();
        let mut convert = 0;
        for e in index.entries() {
            stack
                .at_entry(e.path(&index), Some(e.mode))?
                .matching_attributes(&mut out);
            convert += needs_conversion(&out) as usize;
            let key: Vec<String> = out
                .iter_selected()
                .filter_map(|m| {
                    let value = match m.assignment.state {
                        StateRef::Unspecified => return None,
                        StateRef::Set => "set".to_string(),
                        StateRef::Unset => "unset".to_string(),
                        StateRef::Value(v) => v.as_bstr().to_string(),
                    };
                    Some(format!("{}={value}", m.assignment.name.as_str()))
                })
                .collect();
            *combos.entry(key.join(" ")).or_default() += 1;
        }
        println!(
            "{} files; index from tree {:?}, attribute setup {:?}, all lookups {:?}",
            index.entries().len(),
            indexed,
            setup - indexed,
            seq_started.elapsed()
        );
        println!("files needing checkout conversion (Linux defaults): {convert}");
        for (k, n) in combos.iter().filter(|(k, _)| !k.is_empty()) {
            println!("{n:>8}  {k}");
        }
        Ok(())
    })
}
