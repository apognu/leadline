use std::{fmt, panic::Location};

use sea_orm::{DbBackend, Statement, Value};

use crate::{
  classify::StmtKind,
  expectation::describe,
  matcher::{Arg, SqlMatcher},
};

/// Why one expectation rejected a statement: the first of its checks that
/// failed (see `Spec::check`).
///
/// Its `Display` is the plain-text reason, used in [`MockError::problems`]:
///
/// ```text
/// expected a DELETE statement, got SELECT
/// it targets `bakery`, not `cake`
/// it does not match SQL containing `ORDER BY`
/// arguments [Int(Some(8))] do not match expected [Int(Some(7))]
/// ```
///
/// [`MockError::problems`]: crate::MockError::problems
#[derive(Clone)]
pub(crate) enum Mismatch {
  /// The statement is of another kind than expected: for example, a `SELECT`
  /// received where a `DELETE` was expected.
  Kind { expected: StmtKind, actual: StmtKind },
  /// The statement is not on the expected table.
  ///
  /// `actual` is the table the statement is on, read from its parsed SQL:
  /// `cake` for `SELECT … FROM "cake"`. It is `None` when no table could be
  /// read (the SQL does not parse, or names no table, as `SELECT 1`) and the
  /// SQL does not even mention the expected table. `backend` tells how `actual`
  /// is quoted in the SQL (`"cake"`, or `` `cake` `` on MySQL), to find it
  /// there and point at it.
  Table { expected: String, actual: Option<String>, backend: DbBackend },
  /// One of the expectation's SQL matchers (`matching`, `sql_contains`, …)
  /// rejected the statement: `matcher` is the first one that did.
  ///
  /// `hint` is a likely cause, when one is known. For now, there is one: when
  /// the statement only differs from the one given to `matching(..)` by its
  /// `LIMIT` or `OFFSET`, as `.one()` and paginators add them, the hint
  /// suggests `matching_ignoring_limit`.
  Sql { matcher: Box<SqlMatcher>, hint: Option<&'static str> },
  /// The bound values do not match the arguments of `with_args`.
  ///
  /// `wrong` holds the positions of the values that do not match, when there
  /// are as many values as arguments: with `with_args((7, Any))` and the values
  /// `[8, 'Lemon']`, it is `[0]`. It is empty when they are not as many.
  /// The positions are found while matching, because finding them again when
  /// rendering the failure would call the user's `Arg::matching` closures a
  /// second time.
  Args { expected: Vec<Arg>, actual: Vec<Value>, wrong: Vec<usize> },
}

impl fmt::Display for Mismatch {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Mismatch::Kind { expected, actual } => {
        let article = if matches!(expected, StmtKind::Insert | StmtKind::Update) { "an" } else { "a" };

        write!(f, "expected {article} {expected} statement, got {actual}")
      }
      // Tables are named as in labels, not quoted as in the SQL.
      Mismatch::Table { expected, actual: Some(actual), .. } => write!(f, "it targets `{actual}`, not `{expected}`"),
      Mismatch::Table { expected, actual: None, .. } => write!(f, "it does not reference table `{expected}`"),
      Mismatch::Sql { matcher, hint: None } => write!(f, "it does not match {matcher}"),
      Mismatch::Sql { matcher, hint: Some(hint) } => write!(f, "it does not match {matcher} ({hint})"),
      Mismatch::Args { expected, actual, .. } => write!(f, "arguments {actual:?} do not match expected {expected:?}"),
    }
  }
}

/// An expectation, as failures describe it, taken when the failure happens
/// (the expectation itself keeps changing as calls are recorded).
#[derive(Clone)]
pub(crate) struct Declared {
  /// What the expectation accepts, and how many calls it had:
  /// ``SELECT on `cake` with any SQL (1/2 calls)`` (see `Label`).
  pub label: String,
  /// Where the test declared the expectation: the `mock.expect_*()` call
  /// (`expect_select`, `expect_begin`, …), which failures point at.
  pub location: &'static Location<'static>,
}

/// An expectation that rejected a statement, and why it did.
#[derive(Clone)]
pub(crate) struct Rejection {
  pub expectation: Declared,
  pub mismatch: Mismatch,
}

/// Why no expectation answered a statement.
#[derive(Clone)]
pub(crate) enum Reason {
  /// In an ordered mock, the next pending expectation rejected the statement,
  /// and could not be skipped, as it was not satisfied yet.
  Next(Rejection),
  /// The mock has no expectations at all.
  NoExpectation,
  /// No expectation can be called anymore: each one was already called as
  /// many times as it can be, or was skipped by an ordered mock, which closes
  /// the optional expectations it skips.
  Consumed,
  /// Every pending expectation was compared to the statement, and rejected
  /// it.
  ///
  /// In an unordered mock, that is the usual failure, and `candidates` is
  /// `"pending"`. In an ordered mock, it only happens when every pending
  /// expectation was already satisfied (optional, or `times(n)` with enough
  /// calls), so that each one could be skipped to try the next, and
  /// `candidates` is `"remaining optional"`. `candidates` completes the
  /// sentence "none of the … expectations matches it".
  NoneMatched { candidates: &'static str, rejections: Vec<Rejection> },
  /// An expectation matched the statement, but it has no result to answer it
  /// with: its builder was dropped before `returning(..)`, `rows_affected(..)`
  /// or another method completing it was called.
  MatchedWithoutResult(Declared),
  /// A write that reads the rows it wrote back, such as `INSERT … RETURNING`
  /// on Postgres, matched an expectation that has no rows to return: it was
  /// only given an exec result, such as `rows_affected(..)` or
  /// `exec_with(..)`, which answers writes that do not read rows back.
  ///
  /// `fix` names the methods that would give it rows, to complete the help
  /// message: ``"`.returning(..)`, `.last_insert_id(..)` or
  /// `.last_insert_key(..)`"``.
  MissingRows { fix: &'static str },
}

impl Reason {
  /// A short summary of the reason, which does not name the expectations
  /// involved: `"none of the pending expectations matches it"`.
  ///
  /// It is used in the headline of a failure, and starts the full reason that
  /// `Display` gives.
  pub(crate) fn summary(&self) -> String {
    match self {
      Reason::Next(_) => "the next expectation does not match it".to_string(),
      Reason::NoExpectation => "no expectation was set".to_string(),
      Reason::Consumed => "every expectation was already consumed".to_string(),
      Reason::NoneMatched { candidates, .. } => format!("none of the {candidates} expectations matches it"),
      Reason::MatchedWithoutResult(_) => "it matches an expectation that has no result".to_string(),
      Reason::MissingRows { .. } => "it reads the written rows back (`RETURNING` on this backend), but its expectation has no rows to return".to_string(),
    }
  }
}

/// The full reason: the summary, followed by the expectations involved, and
/// why each one rejected the statement:
///
/// ```text
/// the next expectation does not match it (DELETE on `cake` with any SQL): expected a DELETE statement, got SELECT
/// none of the pending expectations matches it:
///       - DELETE on `cake` with any SQL: expected a DELETE statement, got SELECT
///       - UPDATE on `cake` with any SQL: expected an UPDATE statement, got SELECT
/// ```
impl fmt::Display for Reason {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}", self.summary())?;

    match self {
      Reason::Next(rejection) => write!(f, " ({}): {}", rejection.expectation.label, rejection.mismatch),
      Reason::NoExpectation | Reason::Consumed => Ok(()),
      Reason::NoneMatched { rejections, .. } => {
        write!(f, ":")?;

        for rejection in rejections {
          write!(f, "\n      - {}: {}", rejection.expectation.label, rejection.mismatch)?;
        }

        Ok(())
      }
      Reason::MatchedWithoutResult(expectation) => write!(f, ": {}", expectation.label),
      Reason::MissingRows { fix } => write!(f, ": complete it with {fix}"),
    }
  }
}

/// A problem the mock found.
///
/// An unexpected statement is reported as soon as it arrives, by panicking.
/// Every problem, unexpected statements included, is reported again by
/// `MockDb::check`, and by the check run when the mock is dropped, unless
/// `check` already reported it. That catches the panics that were swallowed,
/// for example by a spawned task.
///
/// Its `Display` is the plain-text message that [`MockError::problems`]
/// returns:
///
/// ```text
/// unexpected SELECT `SELECT "cake"."id" FROM "cake"` with []: no expectation was set
/// expectation not met: DELETE on `cake` with any SQL
/// ```
///
/// [`MockError::problems`]: crate::MockError::problems
#[derive(Clone)]
pub(crate) enum Problem {
  /// A statement that no expectation answered, of the `kind` the mock read
  /// from its SQL.
  Unexpected { kind: StmtKind, stmt: Statement, reason: Reason },
  /// An expectation called fewer times than required: never, for a plain
  /// expectation, or fewer than `n` times after `times(n)`.
  Unmet(Declared),
  /// An expectation without a result: its builder was dropped before a method
  /// completing it, such as `returning(..)`, was called.
  NoResult(Declared),
}

impl Problem {
  /// A one-line summary of the problem.
  ///
  /// For an unexpected statement, it gives the statement's kind and the
  /// reason's summary, but neither the statement nor the expectations
  /// involved, which take several lines. For an expectation not met or
  /// without a result, it is the full message, which fits on one line:
  ///
  /// ```text
  /// unexpected SELECT: none of the pending expectations matches it
  /// expectation not met: DELETE on `cake` with any SQL
  /// ```
  ///
  /// Panics start with it, before the full diagnostic, so that the first line
  /// stays short and plain, for `#[should_panic(expected = …)]` and searches.
  pub(crate) fn headline(&self) -> String {
    match self {
      Problem::Unexpected { kind, reason, .. } => format!("unexpected {kind}: {}", reason.summary()),
      _ => self.to_string(),
    }
  }
}

impl fmt::Display for Problem {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Problem::Unexpected { kind, stmt, reason } => write!(f, "unexpected {kind} {}: {reason}", describe(stmt)),
      Problem::Unmet(expectation) => write!(f, "expectation not met: {}", expectation.label),
      Problem::NoResult(expectation) => write!(f, "expectation has no result: {}", expectation.label),
    }
  }
}
