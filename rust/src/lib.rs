//! lmloop Rust rewrite, milestone 1: native run store, ownership lease,
//! diagnostic gate, and the CLI over them. No worker, no model, no Git writes.

pub mod cli;
pub mod gate;
pub mod git;
pub mod lease;
pub mod operator;
pub mod protocol;
pub mod serial;
pub mod state;
pub mod tui;
pub mod web;
