//! Local search (spec §8): a Gmail-compatible query language compiled to
//! SQL over the message tables and the FTS5 index.

mod exec;
mod parse;

pub use exec::{MAX_RESULTS, search};
pub use parse::{Flag, Mailbox, ParseError, SearchExpr, parse};
