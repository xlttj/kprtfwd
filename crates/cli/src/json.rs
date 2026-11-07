//! `kprtfwd status --json`: the format for scripts, documented in the README
//! and in docs section 11. It is not the daemon's snapshot, whose shape
//! changes with the protocol; it changes only with [`VERSION`].

use serde::Serialize;

/// Bumped when a field is removed or changes its meaning; a new field is
/// no such change.
pub const VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct Status<'a> {
    pub version: u32,
    pub paused: bool,
    /// Every context a forward uses, sorted by name.
    pub contexts: Vec<Context<'a>>,
    /// The forwards the table shows, in its order.
    pub forwards: Vec<Forward<'a>>,
}

#[derive(Debug, Serialize)]
pub struct Context<'a> {
    pub name: &'a str,
    /// As in the header line: `ok`, `unreachable (DNS)`, `login needed`, ...
    pub state: String,
}

#[derive(Debug, Serialize)]
pub struct Forward<'a> {
    pub id: &'a str,
    pub context: &'a str,
    pub namespace: &'a str,
    pub service: &'a str,
    pub remote_port: u16,
    pub local_port: u16,
    pub enabled: bool,
    /// The STATE column: `ready`, `waiting`, `error`, `paused` or `off`.
    pub state: &'static str,
    /// The INFO column; empty when there is nothing to say.
    pub info: String,
}
