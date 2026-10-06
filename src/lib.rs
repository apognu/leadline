// The crate documentation is the README. Its examples are fragments (they use
// entities defined elsewhere), so they are not collected as doctests; every
// public item has its own tested examples.
#![cfg_attr(not(doctest), doc = include_str!("../README.md"))]
#![warn(missing_docs, unreachable_pub)]

mod builders;
mod classify;
mod diff;
mod expectation;
mod matcher;
mod mock;
mod parse;
mod problem;
mod render;

pub use builders::{ExecExpectation, ExecResponse, QueryExpectation, QueryResponse, SelectExpectation, TransactionExpectation};
pub use matcher::{Any, Arg, IntoArgs, StatementExt};
pub use mock::{MockDb, MockError};
