use std::{
  fmt,
  sync::{Arc, LazyLock},
};

use regex::Regex;
use sea_orm::{DbBackend, Statement, Value, sea_query::ValueType};
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

use crate::parse::{Parsed, dialect, parse};

type StmtPredicate = Arc<dyn Fn(&Statement) -> bool + Send + Sync>;
type ValuePredicate = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

/// How the SQL text of an incoming statement is compared to an expectation.
#[derive(Clone)]
pub enum SqlMatcher {
  /// The same SQL, ignoring formatting (see `normalize`).
  Exact(String),
  /// SQL matching a regular expression, formatting included.
  Regex(Regex),
  /// SQL containing a fragment, ignoring formatting.
  Contains(String),
  /// The same statement: the same SQL, ignoring formatting, and the same bound
  /// values. With `ignore_limit`, `LIMIT` and `OFFSET` are ignored (see
  /// `same_unpaginated`); `unpaginated` holds the expected statement without
  /// them, computed once, when the expectation is set up.
  Statement {
    stmt: Statement,
    ignore_limit: bool,
    unpaginated: Option<(String, Vec<Value>)>,
  },
  /// Arbitrary predicate.
  Fn(StmtPredicate),
}

impl SqlMatcher {
  /// A matcher for statements equal to `stmt`, with or without their `LIMIT`
  /// and `OFFSET`. `stmt` is parsed once, here, rather than for every incoming
  /// statement.
  pub(crate) fn statement(stmt: Statement, ignore_limit: bool) -> Self {
    let unpaginated = parse(&stmt).map(|parsed| parsed.unpaginated);

    SqlMatcher::Statement { stmt, ignore_limit, unpaginated }
  }

  /// Whether `stmt` matches. `parsed` is `stmt` parsed, if it parses: the mock
  /// parses each statement once, when it arrives.
  pub(crate) fn matches(&self, stmt: &Statement, parsed: Option<&Parsed>) -> bool {
    match self {
      SqlMatcher::Exact(sql) => normalize(sql, stmt.db_backend) == normalize(&stmt.sql, stmt.db_backend),
      SqlMatcher::Regex(re) => re.is_match(&stmt.sql),
      SqlMatcher::Contains(needle) => normalize(&stmt.sql, stmt.db_backend).contains(&normalize(needle, stmt.db_backend)),
      SqlMatcher::Statement {
        stmt: expected, ignore_limit: false, ..
      } => normalize(&expected.sql, expected.db_backend) == normalize(&stmt.sql, stmt.db_backend) && expected.args() == stmt.args(),
      SqlMatcher::Statement {
        stmt: expected,
        ignore_limit: true,
        unpaginated,
      } => same_unpaginated(expected, unpaginated.as_ref(), stmt, parsed),
      SqlMatcher::Fn(predicate) => predicate(stmt),
    }
  }

  /// A likely cause of a mismatch, added to the failure message. For now, only
  /// a statement that differs from the expected one by its `LIMIT` or `OFFSET`
  /// gets one, suggesting `matching_ignoring_limit`.
  pub(crate) fn hint(&self, stmt: &Statement, parsed: Option<&Parsed>) -> Option<&'static str> {
    match self {
      SqlMatcher::Statement {
        stmt: expected,
        ignore_limit: false,
        unpaginated,
      } if same_unpaginated(expected, unpaginated.as_ref(), stmt, parsed) => Some(
        " (they only differ by LIMIT/OFFSET, which `.one()` and paginators add: \
         use `matching_ignoring_limit`)",
      ),
      _ => None,
    }
  }
}

/// Whether two statements are equal once their `LIMIT` and `OFFSET` clauses,
/// and the values bound to them, are removed:
///
/// ```text
/// expected   SELECT … WHERE "id" = $1             [1]
/// actual     SELECT … WHERE "id" = $1 LIMIT $2    [1, 1]
/// compared   SELECT … WHERE "id" = $1             [1]      → equal
/// ```
///
/// `expected_unpaginated` and `parsed` are the two statements, already parsed
/// if they parse.
///
/// - If both statements parse, `LIMIT` and `OFFSET` are removed from the parsed
///   statements, wherever they are. That includes a `LIMIT` followed by a row
///   lock, as in `SELECT … LIMIT $2 FOR UPDATE`, which SeaORM sends for
///   `.lock_exclusive().one(db)`.
/// - Otherwise, `LIMIT` and `OFFSET` are only removed when they end the SQL
///   (see `strip_trailing_pagination`). They are removed this way from both
///   statements, so that the two are still compared the same way.
fn same_unpaginated(expected: &Statement, expected_unpaginated: Option<&(String, Vec<Value>)>, stmt: &Statement, parsed: Option<&Parsed>) -> bool {
  match (expected_unpaginated, parsed) {
    (Some(expected), Some(actual)) => *expected == actual.unpaginated,
    _ => strip_trailing_pagination(expected) == strip_trailing_pagination(stmt),
  }
}

/// Remove the `LIMIT` and `OFFSET` clauses at the end of a statement, with the
/// values bound to them: `… WHERE "id" = $1 LIMIT $2 OFFSET $3` with `[1, 10,
/// 20]` becomes `… WHERE "id" = $1` with `[1]`.
///
/// This is the fallback for statements that do not parse, and only removes
/// `LIMIT` and `OFFSET` when they end the SQL. SQL allows a row lock after them,
/// as in `SELECT … LIMIT $2 FOR UPDATE`: that `LIMIT` is kept.
fn strip_trailing_pagination(stmt: &Statement) -> (String, Vec<Value>) {
  static TRAILING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+(?:LIMIT|OFFSET)\s+(\$\d+|\?|\d+)\s*$").unwrap());

  let mut sql = normalize(&stmt.sql, stmt.db_backend);
  let mut values = stmt.args().to_vec();

  while let Some(captures) = TRAILING.captures(&sql) {
    // Placeholders are bound last, literals are not bound at all.
    if !captures[1].starts_with(|c: char| c.is_ascii_digit()) {
      values.pop();
    }

    sql.truncate(captures.get(0).unwrap().start());
  }

  (sql, values)
}

impl fmt::Display for SqlMatcher {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      SqlMatcher::Exact(sql) => write!(f, "SQL equal to `{}`", normalize_text(sql)),
      SqlMatcher::Regex(re) => write!(f, "SQL matching /{re}/"),
      SqlMatcher::Contains(needle) => write!(f, "SQL containing `{needle}`"),
      SqlMatcher::Statement { stmt, ignore_limit, .. } => {
        write!(f, "statement `{}` with {:?}", normalize(&stmt.sql, stmt.db_backend), stmt.args())?;

        if *ignore_limit {
          write!(f, " (ignoring LIMIT/OFFSET)")?;
        }

        Ok(())
      }
      SqlMatcher::Fn(_) => write!(f, "SQL accepted by a custom predicate"),
    }
  }
}

/// Accepts any value for one argument, in [`with_args`](crate::SelectExpectation::with_args).
///
/// For arguments a test does not care about, or cannot predict, such as
/// generated identifiers or timestamps, while still checking the others and
/// the number of arguments.
///
/// ```
/// # include!("../doctests/entities.rs");
/// use sea_orm::{ColumnTrait, DbBackend, EntityTrait, QueryFilter, sea_query::Expr};
/// use leadline::{Any, MockDb};
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let mock = MockDb::new(DbBackend::Postgres);
///
/// mock.expect_update::<cake::Entity>().with_args((Any, 1)).rows_affected(1);
///
/// let db = mock.connection().await;
/// cake::Entity::update_many()
///   .col_expr(cake::Column::Name, Expr::value(format!("Cake {}", rand_suffix())))
///   .filter(cake::Column::Id.eq(1))
///   .exec(&db)
///   .await
///   .unwrap();
/// # fn rand_suffix() -> u32 { 4 }
/// # }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Any;

/// Matcher for one argument in [`with_args`](crate::SelectExpectation::with_args).
///
/// Plain values and [`Any`] are usually enough; build an `Arg` with
/// [`Arg::matching`] for a custom check.
#[derive(Clone)]
pub struct Arg(ArgMatcher);

#[derive(Clone)]
enum ArgMatcher {
  /// Equal to this value. Integers compare by value whatever their width (see
  /// `integer`); other values must have the same type.
  Eq(Value),
  Any,
  Fn(ValuePredicate),
}

impl Arg {
  /// Accept an argument for which `predicate` returns `true`.
  ///
  /// For arguments that cannot be compared exactly, but still have properties
  /// worth checking: a range, a format, or several acceptable values.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, Value};
  /// use leadline::{Arg, MockDb};
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// let positive = Arg::matching(|value| matches!(value, Value::Int(Some(id)) if *id > 0));
  ///
  /// mock.expect_delete::<cake::Entity>().with_args((positive,)).rows_affected(1);
  ///
  /// let db = mock.connection().await;
  /// cake::Entity::delete_by_id(12).exec(&db).await.unwrap();
  /// # }
  /// ```
  pub fn matching(predicate: impl Fn(&Value) -> bool + Send + Sync + 'static) -> Self {
    Arg(ArgMatcher::Fn(Arc::new(predicate)))
  }

  fn matches(&self, value: &Value) -> bool {
    match &self.0 {
      ArgMatcher::Eq(expected) => expected == value || matches!((integer(expected), integer(value)), (Some(lhs), Some(rhs)) if lhs == rhs),
      ArgMatcher::Any => true,
      ArgMatcher::Fn(predicate) => predicate(value),
    }
  }
}

impl fmt::Debug for Arg {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match &self.0 {
      ArgMatcher::Eq(value) => write!(f, "{value:?}"),
      ArgMatcher::Any => write!(f, "<any>"),
      ArgMatcher::Fn(_) => write!(f, "<predicate>"),
    }
  }
}

/// A value accepted for one argument by
/// [`with_args`](crate::SelectExpectation::with_args):
///
/// - anything convertible into a [`Value`], which matches an equal value;
/// - [`Any`], which matches any value;
/// - an [`Arg`], for a custom check.
///
/// This trait is sealed: it cannot be implemented outside of leadline.
pub trait IntoArg: sealed::IntoArg {}

impl<T: sealed::IntoArg> IntoArg for T {}

/// A list of arguments accepted by
/// [`with_args`](crate::SelectExpectation::with_args):
///
/// - a tuple of up to 8 [`IntoArg`]s, which may have different types: `(1, Any,
///   "Lemon")`. A tuple of one argument needs a trailing comma: `(1,)`;
/// - a `Vec` of one [`IntoArg`] type, for long lists: `vec![1, 2, 3, 4]`;
/// - `()`, for no arguments.
///
/// This trait is sealed: it cannot be implemented outside of leadline.
///
/// ```
/// # include!("../doctests/entities.rs");
/// use sea_orm::DbBackend;
/// use leadline::{Any, MockDb};
///
/// # fn main() {
/// let mock = MockDb::new(DbBackend::Postgres);
///
/// // A tuple, mixing values and matchers.
/// mock.expect_update::<cake::Entity>().with_args(("Lemon", Any)).maybe().rows_affected(1);
/// // A `Vec` of values of one type.
/// mock.expect_delete::<cake::Entity>().with_args(vec![1, 2, 3, 4]).maybe().rows_affected(4);
/// // No arguments.
/// mock.expect_delete::<cake::Entity>().with_args(()).maybe().rows_affected(0);
/// # }
/// ```
pub trait IntoArgs: sealed::IntoArgs {}

impl<T: sealed::IntoArgs> IntoArgs for T {}

/// The conversions behind [`IntoArg`] and [`IntoArgs`], kept out of the
/// public API.
pub(crate) mod sealed {
  use sea_orm::Value;

  use super::{Any, Arg, ArgMatcher};

  pub trait IntoArg {
    fn into_arg(self) -> Arg;
  }

  impl<T: Into<Value>> IntoArg for T {
    fn into_arg(self) -> Arg {
      Arg(ArgMatcher::Eq(self.into()))
    }
  }

  impl IntoArg for Any {
    fn into_arg(self) -> Arg {
      Arg(ArgMatcher::Any)
    }
  }

  impl IntoArg for Arg {
    fn into_arg(self) -> Arg {
      self
    }
  }

  pub trait IntoArgs {
    fn into_args(self) -> Vec<Arg>;
  }

  impl IntoArgs for () {
    fn into_args(self) -> Vec<Arg> {
      Vec::new()
    }
  }

  impl<T: IntoArg> IntoArgs for Vec<T> {
    fn into_args(self) -> Vec<Arg> {
      self.into_iter().map(IntoArg::into_arg).collect()
    }
  }

  macro_rules! impl_into_args {
    ($($name:ident),+) => {
      impl<$($name: IntoArg),+> IntoArgs for ($($name,)+) {
        #[allow(non_snake_case)]
        fn into_args(self) -> Vec<Arg> {
          let ($($name,)+) = self;
          vec![$($name.into_arg()),+]
        }
      }
    };
  }

  impl_into_args!(A);
  impl_into_args!(A, B);
  impl_into_args!(A, B, C);
  impl_into_args!(A, B, C, D);
  impl_into_args!(A, B, C, D, E);
  impl_into_args!(A, B, C, D, E, F);
  impl_into_args!(A, B, C, D, E, F, G);
  impl_into_args!(A, B, C, D, E, F, G, H);
}

/// The value of an integer, whatever its type: `Value::Int(Some(1))` and
/// `Value::BigUnsigned(Some(1))` both give `Some(Some(1))`. An integer `NULL`
/// gives `Some(None)`, and a value of any other type `None`.
///
/// This lets `with_args((1,))` match a `BigInt` column, although a Rust integer
/// literal is bound as a `Value::Int`.
pub(crate) fn integer(value: &Value) -> Option<Option<i128>> {
  Some(match value {
    Value::TinyInt(n) => n.map(i128::from),
    Value::SmallInt(n) => n.map(i128::from),
    Value::Int(n) => n.map(i128::from),
    Value::BigInt(n) => n.map(i128::from),
    Value::TinyUnsigned(n) => n.map(i128::from),
    Value::SmallUnsigned(n) => n.map(i128::from),
    Value::Unsigned(n) => n.map(i128::from),
    Value::BigUnsigned(n) => n.map(i128::from),
    _ => return None,
  })
}

pub(crate) fn args_match(expected: &[Arg], stmt: &Statement) -> bool {
  let actual = stmt.args();

  expected.len() == actual.len() && expected.iter().zip(actual).all(|(arg, value)| arg.matches(value))
}

/// Typed access to the values bound to a [`Statement`].
///
/// Closures computing results, such as
/// [`returning_with`](crate::SelectExpectation::returning_with), receive the
/// incoming statement. SeaORM stores its values as an optional list of untyped
/// [`Value`]s, so reading the first one as a `String` takes
/// `stmt.values.as_ref().map(|values| values.0[0].clone())`, then matching
/// `Value::String(Some(…))`. With this trait, it is `stmt.arg::<String>(0)`.
pub trait StatementExt {
  /// The values bound to the statement, in order, or an empty slice when
  /// there are none.
  ///
  /// ```
  /// use leadline::StatementExt;
  /// use sea_orm::{DbBackend, Statement, Value};
  ///
  /// let stmt = Statement::from_sql_and_values(DbBackend::Postgres, r#"SELECT * FROM "cake" WHERE "id" = $1"#, [1.into()]);
  /// assert_eq!(stmt.args(), [Value::Int(Some(1))]);
  ///
  /// let stmt = Statement::from_string(DbBackend::Postgres, "SELECT 1");
  /// assert!(stmt.args().is_empty());
  /// ```
  fn args(&self) -> &[Value];

  /// The value bound at `index` (from 0), converted to `T`.
  ///
  /// Returns `None` when there is no value at `index`, or when it does not
  /// convert to `T`: a value of another type, or a `NULL` unless `T` is an
  /// `Option`.
  ///
  /// ```
  /// use leadline::StatementExt;
  /// use sea_orm::{DbBackend, Statement, Value};
  ///
  /// let stmt = Statement::from_sql_and_values(
  ///   DbBackend::Postgres,
  ///   r#"UPDATE "cake" SET "name" = $1, "bakery_id" = $2 WHERE "id" = $3"#,
  ///   [Value::from("Lemon"), Value::Int(None), Value::from(7)],
  /// );
  ///
  /// assert_eq!(stmt.arg::<String>(0).as_deref(), Some("Lemon"));
  /// assert_eq!(stmt.arg::<Option<i32>>(1), Some(None));
  /// assert_eq!(stmt.arg::<i32>(2), Some(7));
  /// assert_eq!(stmt.arg::<String>(2), None);
  /// assert_eq!(stmt.arg::<i32>(3), None);
  /// ```
  fn arg<T: ValueType>(&self, index: usize) -> Option<T>;
}

impl StatementExt for Statement {
  fn args(&self) -> &[Value] {
    self.values.as_ref().map(|values| values.0.as_slice()).unwrap_or_default()
  }

  fn arg<T: ValueType>(&self, index: usize) -> Option<T> {
    self.args().get(index).cloned().and_then(|value| T::try_from(value).ok())
  }
}

/// Normalize the formatting of SQL, so that statements differing only by
/// whitespace compare equal: each run of whitespace becomes a single space, and
/// leading and trailing whitespace is removed.
///
/// ```text
/// SELECT  *
///   FROM "cake"          →   SELECT * FROM "cake"
/// ```
///
/// Only the whitespace between tokens changes. The SQL is tokenized with the
/// dialect of `backend`, and every other token is copied as is: string literals
/// (`'a   b'`, `$$a   b$$`), quoted identifiers and comments keep their own
/// whitespace. A `--` comment also keeps the line break that ends it:
/// otherwise, `SELECT 1 -- note` followed by `FROM cake` on the next line would
/// become `SELECT 1 -- note FROM cake`, where `FROM cake` is part of the
/// comment.
///
/// SQL that does not tokenize, such as a fragment ending inside a quote, is
/// normalized by `normalize_text` instead.
pub(crate) fn normalize(sql: &str, backend: DbBackend) -> String {
  let Ok(tokens) = Tokenizer::new(dialect(backend).as_ref(), sql).tokenize_with_location() else {
    return normalize_text(sql);
  };

  let tokens: Vec<_> = tokens.into_iter().filter(|token| token.token != Token::EOF).collect();
  let offsets = token_offsets(sql, tokens.iter().map(|token| (token.span.start.line, token.span.start.column)));

  let mut normalized = String::with_capacity(sql.len());
  let mut space = false;
  let mut after_line_comment = false;

  for (index, token) in tokens.iter().enumerate() {
    match &token.token {
      Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab) => space = true,
      token => {
        if after_line_comment {
          normalized.push('\n');
        } else if space && !normalized.is_empty() {
          normalized.push(' ');
        }

        // Tokens are contiguous: each one runs until the next one starts.
        let end = offsets.get(index + 1).copied().unwrap_or(sql.len());
        normalized.push_str(sql[offsets[index]..end].trim_end());

        space = false;
        after_line_comment = matches!(token, Token::Whitespace(Whitespace::SingleLineComment { .. }));
      }
    }
  }

  normalized
}

/// Convert token positions, given as a line and a column, both starting at 1
/// and counted in characters, into byte offsets in `sql`. Positions must come
/// in order. In `"SELECT\n  1"`, line 2, column 3 (the `1`) is at byte 9.
fn token_offsets(sql: &str, positions: impl Iterator<Item = (u64, u64)>) -> Vec<usize> {
  let mut chars = sql.char_indices().peekable();
  let (mut line, mut column) = (1, 1);

  positions
    .map(|position| {
      while (line, column) != position {
        match chars.next() {
          Some((_, '\n')) => (line, column) = (line + 1, 1),
          Some(_) => column += 1,
          None => return sql.len(),
        }
      }

      chars.peek().map_or(sql.len(), |(offset, _)| *offset)
    })
    .collect()
}

/// The fallback of `normalize` for SQL that does not tokenize, such as a
/// fragment ending inside a quote.
///
/// It collapses whitespace outside quotes, and keeps everything inside `'…'`
/// (where `''` is an escaped quote), `"…"` and `` `…` `` as is. Unlike
/// `normalize`, it knows nothing of comments, nor of the literals specific to a
/// backend.
fn normalize_text(sql: &str) -> String {
  let mut normalized = String::with_capacity(sql.len());
  let mut chars = sql.chars().peekable();
  let mut space = false;

  while let Some(c) = chars.next() {
    if c.is_whitespace() {
      space = true;
      continue;
    }

    if space && !normalized.is_empty() {
      normalized.push(' ');
    }

    space = false;
    normalized.push(c);

    if matches!(c, '\'' | '"' | '`') {
      while let Some(quoted) = chars.next() {
        normalized.push(quoted);

        if quoted == c {
          // A doubled quote is an escaped one, and does not end the quote.
          match chars.next_if_eq(&c) {
            Some(escaped) => normalized.push(escaped),
            None => break,
          }
        }
      }
    }
  }

  normalized
}

#[cfg(test)]
mod tests {
  use sea_orm::{DbBackend, Values};

  use super::{sealed::IntoArgs as _, *};

  fn stmt(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
  }

  #[test]
  fn sql_matchers() {
    let s = stmt("SELECT  *\n FROM \"cake\" WHERE id = $1", vec![1i32.into()]);

    assert!(SqlMatcher::Exact(r#"SELECT * FROM "cake" WHERE id = $1"#.into()).matches(&s, parse(&s).as_ref()));
    assert!(!SqlMatcher::Exact("SELECT 1".into()).matches(&s, parse(&s).as_ref()));
    assert!(SqlMatcher::Regex(Regex::new(r#"FROM\s+"cake""#).unwrap()).matches(&s, parse(&s).as_ref()));
    assert!(SqlMatcher::Contains(r#"FROM "cake""#.into()).matches(&s, parse(&s).as_ref()));
    assert!(SqlMatcher::Fn(Arc::new(|s| s.sql.contains("cake"))).matches(&s, parse(&s).as_ref()));

    let same = stmt(r#"SELECT * FROM "cake" WHERE id = $1"#, vec![1i32.into()]);
    let other = stmt(r#"SELECT * FROM "cake" WHERE id = $1"#, vec![2i32.into()]);
    let strict = |stmt| SqlMatcher::statement(stmt, false);
    assert!(strict(same).matches(&s, parse(&s).as_ref()));
    assert!(!strict(other).matches(&s, parse(&s).as_ref()));
  }

  #[test]
  fn statement_ignoring_limit() {
    let loose = |stmt| SqlMatcher::statement(stmt, true);
    let expected = stmt(r#"SELECT * FROM "cake" WHERE id = $1"#, vec![1i32.into()]);

    let paginated = stmt(r#"SELECT * FROM "cake" WHERE id = $1 LIMIT $2 OFFSET $3"#, vec![1i32.into(), 10u64.into(), 20u64.into()]);
    let literal = stmt(r#"SELECT * FROM "cake" WHERE id = $1 LIMIT 1"#, vec![1i32.into()]);
    let other = stmt(r#"SELECT * FROM "cake" WHERE id = $1 LIMIT $2"#, vec![2i32.into(), 1u64.into()]);

    assert!(loose(expected.clone()).matches(&paginated, parse(&paginated).as_ref()));
    assert!(loose(expected.clone()).matches(&literal, parse(&literal).as_ref()));
    assert!(!loose(expected.clone()).matches(&other, parse(&other).as_ref()));

    let strict = SqlMatcher::statement(expected, false);
    assert!(!strict.matches(&paginated, parse(&paginated).as_ref()));
    assert!(strict.hint(&paginated, parse(&paginated).as_ref()).is_some());
    assert!(strict.hint(&other, parse(&other).as_ref()).is_none());
  }

  #[test]
  fn normalization_keeps_quoted_whitespace() {
    let pg = |sql| normalize(sql, DbBackend::Postgres);

    assert_eq!(pg("  SELECT  'a   b' ,\n\t\"my  col\"  FROM  t "), r#"SELECT 'a   b' , "my  col" FROM t"#);
    assert_eq!(pg("SELECT 'it''s   ok'   FROM t"), "SELECT 'it''s   ok' FROM t");
    assert_eq!(normalize("SELECT `a  b`  FROM t", DbBackend::MySql), "SELECT `a  b` FROM t");

    // The fallback scanner, for SQL that does not tokenize.
    assert_eq!(normalize_text("  SELECT  'a   b' ,\n  x "), "SELECT 'a   b' , x");
    assert_eq!(pg("SELECT 'unterminated   x"), "SELECT 'unterminated   x");

    let s = stmt("SELECT 'a   b'", vec![]);
    assert!(!SqlMatcher::Exact("SELECT 'a b'".into()).matches(&s, parse(&s).as_ref()));
    assert!(SqlMatcher::Exact("SELECT   'a   b'".into()).matches(&s, parse(&s).as_ref()));
    assert!(!SqlMatcher::Contains("'a b'".into()).matches(&s, parse(&s).as_ref()));
  }

  #[test]
  fn normalization_follows_the_dialect() {
    let pg = |sql| normalize(sql, DbBackend::Postgres);
    let mysql = |sql| normalize(sql, DbBackend::MySql);

    // A line comment must not swallow what follows its line break.
    assert_ne!(pg("SELECT 1 -- comment\nFROM cake"), pg("SELECT 1 -- comment FROM cake"));
    assert_eq!(pg("SELECT 1 -- comment\n   FROM   cake"), pg("SELECT 1 -- comment\nFROM cake"));
    assert_eq!(pg("SELECT 1 /* a   b */  FROM cake"), "SELECT 1 /* a   b */ FROM cake");

    // Dollar-quoted strings (Postgres) and backslash escapes (MySQL) keep
    // their whitespace.
    assert_ne!(pg("SELECT $$a b$$"), pg("SELECT $$a   b$$"));
    assert_ne!(mysql("SELECT 'it\\'s  x'"), mysql("SELECT 'it\\'s x'"));
    assert_eq!(mysql("SELECT  'it\\'s  x'  FROM t"), mysql("SELECT 'it\\'s  x' FROM t"));

    // Literals are copied verbatim, never re-rendered: one literal holding
    // quotes stays distinct from two literals.
    assert_ne!(pg("SELECT 'x'' || ''y'"), pg("SELECT 'x' || 'y'"));
    assert_eq!(pg("SELECT  'it''s'"), "SELECT 'it''s'");

    let s = stmt("SELECT 1 -- comment FROM cake", vec![]);
    assert!(!SqlMatcher::Exact("SELECT 1 -- comment\nFROM cake".into()).matches(&s, parse(&s).as_ref()));
  }

  #[test]
  fn integer_args_match_whatever_their_width() {
    let s = stmt(
      "SELECT * FROM t WHERE a = $1 AND b = $2 AND c = $3",
      vec![Value::BigInt(Some(1)), Value::BigUnsigned(Some(7)), Value::SmallInt(None)],
    );

    assert!(args_match(&(1, 7u8, None::<i64>).into_args(), &s));
    assert!(!args_match(&(2, 7, None::<i64>).into_args(), &s));
    // Other types still need to match exactly.
    assert!(!args_match(&(1, 7, None::<String>).into_args(), &s));
    assert!(!args_match(&("1", 7, None::<i64>).into_args(), &s));
  }

  #[test]
  fn typed_args() {
    let s = stmt("UPDATE x SET a = $1, b = $2", vec![1i32.into(), Value::String(None)]);

    assert_eq!(s.arg::<i32>(0), Some(1));
    assert_eq!(s.arg::<String>(0), None);
    assert_eq!(s.arg::<String>(1), None);
    assert_eq!(s.arg::<Option<String>>(1), Some(None));
    assert_eq!(s.arg::<i32>(2), None);

    let none = Statement::from_string(DbBackend::Postgres, "SELECT 1");
    assert!(none.args().is_empty());
    assert_eq!(none.arg::<i32>(0), None);
  }

  #[test]
  fn arg_matchers() {
    let s = stmt("UPDATE x SET a = $1, b = $2", vec![1i32.into(), "foo".into()]);

    assert!(args_match(&(1i32, "foo").into_args(), &s));
    assert!(args_match(&(Any, "foo").into_args(), &s));
    assert!(args_match(&(Arg::matching(|v| *v == Value::Int(Some(1))), Any).into_args(), &s));
    assert!(!args_match(&(2i32, "foo").into_args(), &s));
    assert!(!args_match(&(1i32,).into_args(), &s));
    assert!(args_match(
      &().into_args(),
      &Statement {
        values: Some(Values(vec![])),
        ..s.clone()
      }
    ));
  }
}
