//! Hidden benchmark commands (`bench-attrs`, `bench-read`).

// Developer tools printing their measurements; a closed pipe there is no matter.
#![allow(clippy::print_stdout)]

pub mod attrs;
pub mod read;
