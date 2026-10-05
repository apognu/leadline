use std::fmt;

/// The kind of a SQL statement, such as `SELECT` or `INSERT`.
///
/// An expectation only accepts statements of its kind. The mock reads the kind
/// from the parsed statement, so that `WITH old AS (…) DELETE FROM cake …` is a
/// [`Delete`](Self::Delete), not a `SELECT`. When the SQL does not parse, it
/// falls back to [`classify`]. `Display` gives the SQL keyword, for failure
/// messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StmtKind {
  /// A query returning rows: `SELECT`, `WITH` (a common table expression)
  /// or `VALUES`.
  Select,
  /// `INSERT`, or MySQL's `REPLACE`.
  Insert,
  /// `UPDATE`.
  Update,
  /// `DELETE`.
  Delete,
  /// The start of a transaction or savepoint: `BEGIN` or `START`.
  Begin,
  /// The commit of a transaction or savepoint.
  Commit,
  /// The rollback of a transaction or savepoint.
  Rollback,
  /// Anything else: DDL, `TRUNCATE`, vendor-specific commands, ...
  Other,
}

impl StmtKind {
  pub(crate) fn is_transaction(self) -> bool {
    matches!(self, StmtKind::Begin | StmtKind::Commit | StmtKind::Rollback)
  }
}

impl fmt::Display for StmtKind {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let name = match self {
      StmtKind::Select => "SELECT",
      StmtKind::Insert => "INSERT",
      StmtKind::Update => "UPDATE",
      StmtKind::Delete => "DELETE",
      StmtKind::Begin => "BEGIN",
      StmtKind::Commit => "COMMIT",
      StmtKind::Rollback => "ROLLBACK",
      StmtKind::Other => "OTHER",
    };

    f.write_str(name)
  }
}

/// Classify a SQL statement by its first keyword, skipping whitespace, comments
/// and opening parentheses: `/* hint */ (SELECT 1)` is a `SELECT`.
///
/// The mock uses this for statements its parser cannot read: it never fails on
/// unknown syntax. But it is wrong for common table expressions that write, as
/// they start with `WITH` whatever they do: `WITH old AS (…) DELETE …` is
/// classified as a `SELECT`.
pub(crate) fn classify(sql: &str) -> StmtKind {
  let keyword = leading_keyword(sql).to_ascii_uppercase();

  match keyword.as_str() {
    "SELECT" | "WITH" | "VALUES" => StmtKind::Select,
    "INSERT" | "REPLACE" => StmtKind::Insert,
    "UPDATE" => StmtKind::Update,
    "DELETE" => StmtKind::Delete,
    "BEGIN" | "START" => StmtKind::Begin,
    "COMMIT" => StmtKind::Commit,
    "ROLLBACK" => StmtKind::Rollback,
    _ => StmtKind::Other,
  }
}

fn leading_keyword(mut sql: &str) -> &str {
  loop {
    sql = sql.trim_start_matches(|c: char| c.is_whitespace() || c == '(');

    if let Some(rest) = sql.strip_prefix("--") {
      sql = rest.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
    } else if let Some(rest) = sql.strip_prefix("/*") {
      sql = rest.split_once("*/").map(|(_, rest)| rest).unwrap_or("");
    } else {
      break;
    }
  }

  let end = sql.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(sql.len());

  &sql[..end]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn classifies_leading_keyword() {
    assert_eq!(classify(r#"SELECT "id" FROM "cake""#), StmtKind::Select);
    assert_eq!(classify("  with x as (select 1) select * from x"), StmtKind::Select);
    assert_eq!(classify("(SELECT 1) UNION (SELECT 2)"), StmtKind::Select);
    assert_eq!(classify("INSERT INTO cake VALUES (1)"), StmtKind::Insert);
    assert_eq!(classify("update cake set x = 1"), StmtKind::Update);
    assert_eq!(classify("DELETE FROM cake"), StmtKind::Delete);
    assert_eq!(classify("CREATE TABLE cake ()"), StmtKind::Other);
    assert_eq!(classify(""), StmtKind::Other);
  }

  #[test]
  fn skips_comments() {
    assert_eq!(classify("-- hello\nSELECT 1"), StmtKind::Select);
    assert_eq!(classify("/* hint */ DELETE FROM cake"), StmtKind::Delete);
  }
}
