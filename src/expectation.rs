use std::{fmt, panic::Location, sync::Arc};

use sea_orm::{DbBackend, DbErr, IntoMockRow, ProxyExecResult, ProxyRow, Statement};

use crate::{
  classify::StmtKind,
  matcher::{Arg, SqlMatcher, StatementExt, normalize, wrong_args},
  parse::Parsed,
  problem::{Declared, Mismatch, Rejection},
};

pub(crate) type RowsFn = Arc<dyn Fn(&Statement) -> Result<Vec<ProxyRow>, DbErr> + Send + Sync>;
pub(crate) type ExecFn = Arc<dyn Fn(&Statement) -> Result<ProxyExecResult, DbErr> + Send + Sync>;

#[derive(Clone)]
pub(crate) enum Rows {
  Static(Vec<ProxyRow>),
  Fn(RowsFn),
}

#[derive(Clone)]
pub(crate) enum Exec {
  Static(ProxyExecResult),
  Fn(ExecFn),
}

/// A single scripted interaction with the mock database.
///
/// What it matches (`spec`) and what it answers (`response`) are behind `Arc`s,
/// so that they can be copied out of the mock's state, and user closures called
/// without holding its lock.
#[derive(Clone)]
pub(crate) struct Expectation {
  pub spec: Arc<Spec>,
  pub response: Arc<Response>,
  /// Calls needed for the expectation to be met.
  pub min: usize,
  /// Calls after which the expectation stops matching.
  pub max: usize,
  pub calls: usize,
  /// Where the test declared the expectation: the line of its
  /// `mock.expect_*()` call, which failure messages point at.
  pub location: &'static Location<'static>,
}

/// Which statements an expectation accepts.
#[derive(Clone)]
pub(crate) struct Spec {
  /// The kind of statement accepted, or `None` for any kind.
  pub kind: Option<StmtKind>,
  pub table: Option<String>,
  /// All of them must match.
  pub matchers: Vec<SqlMatcher>,
  pub args: Option<Vec<Arg>>,
}

/// What an expectation answers.
#[derive(Clone, Default)]
pub(crate) struct Response {
  pub rows: Option<Rows>,
  pub exec: Option<Exec>,
  /// The row holding the primary key, set by `last_insert_id` or
  /// `last_insert_key`. It answers writes sent with `RETURNING`, such as
  /// inserts on Postgres.
  pub returning_pk: Option<ProxyRow>,
  pub error: Option<DbErr>,
  /// Whether the rows were written out with `returning_rows`, and are returned
  /// as they are. Rows built from models are renamed to the aliases the
  /// statement selects instead.
  pub raw_rows: bool,
}

impl Expectation {
  /// Create an expectation for one statement of `kind`, or of any kind for
  /// `None`, declared by the test at `location`.
  pub(crate) fn new(kind: Option<StmtKind>, location: &'static Location<'static>) -> Self {
    Self {
      spec: Arc::new(Spec {
        kind,
        table: None,
        matchers: Vec::new(),
        args: None,
      }),
      response: Arc::default(),
      min: 1,
      max: 1,
      calls: 0,
      location,
    }
  }

  /// The matching part, for builders to change. Nothing else holds it while
  /// expectations are set up, so this does not copy it.
  pub(crate) fn spec_mut(&mut self) -> &mut Spec {
    Arc::make_mut(&mut self.spec)
  }

  /// The response part, for builders.
  pub(crate) fn response_mut(&mut self) -> &mut Response {
    Arc::make_mut(&mut self.response)
  }

  /// Whether this expectation can still answer statements.
  pub(crate) fn exhausted(&self) -> bool {
    self.calls >= self.max
  }

  /// Whether a result was scripted (transaction boundaries need none). Builders
  /// require one, but a pending builder can be dropped without one.
  pub(crate) fn has_result(&self) -> bool {
    let response = &self.response;

    self.spec.is_transaction() || response.rows.is_some() || response.exec.is_some() || response.error.is_some()
  }

  /// Whether this expectation was called enough.
  pub(crate) fn satisfied(&self) -> bool {
    self.calls >= self.min
  }

  /// Expect `n` calls: exactly `n`, or up to `n` for an optional expectation
  /// (see `maybe`). Works in either order: `.times(3).maybe()` and
  /// `.maybe().times(3)` both allow 0 to 3 calls.
  pub(crate) fn times(&mut self, n: usize) {
    if self.min > 0 {
      self.min = n;
    }

    self.max = n;
  }

  /// Make the expectation optional: it may be called up to its `times` (once
  /// by default), or not at all.
  pub(crate) fn maybe(&mut self) {
    self.min = 0;
  }

  /// Stop matching statements. An ordered mock does this to the optional
  /// expectations it skips.
  pub(crate) fn close(&mut self) {
    self.max = self.calls;
  }

  /// The fixed exec result, to change its fields: created if missing, replacing
  /// any closure.
  pub(crate) fn exec_mut(&mut self) -> &mut ProxyExecResult {
    let response = self.response_mut();

    if !matches!(response.exec, Some(Exec::Static(_))) {
      response.exec = Some(Exec::Static(ProxyExecResult::default()));
    }

    match &mut response.exec {
      Some(Exec::Static(exec)) => exec,
      _ => unreachable!(),
    }
  }

  /// Describe the expectation for failure messages, as it stands now: what it
  /// accepts, how many calls it had, and where the test declared it.
  pub(crate) fn declared(&self) -> Declared {
    Declared {
      label: self.label().to_string(),
      location: self.location,
    }
  }

  /// A description of the expectation, for messages.
  pub(crate) fn label(&self) -> Label<'_> {
    Label(self)
  }

  /// Pair the expectation, as failures describe it, with why it rejected a
  /// statement.
  pub(crate) fn rejection(&self, mismatch: Mismatch) -> Rejection {
    Rejection {
      expectation: self.declared(),
      mismatch,
    }
  }
}

impl Spec {
  /// Whether this expects a transaction boundary (`BEGIN`, `COMMIT` or
  /// `ROLLBACK`).
  pub(crate) fn is_transaction(&self) -> bool {
    self.kind.is_some_and(StmtKind::is_transaction)
  }

  /// Check an incoming statement against this expectation, returning why it
  /// does not match. This runs user closures (`sql_fn`, `Arg::matching`), so
  /// it must be called without holding the mock's lock.
  pub(crate) fn check(&self, kind: StmtKind, stmt: &Statement, parsed: Option<&Parsed>) -> Result<(), Mismatch> {
    if let Some(expected) = self.kind
      && expected != kind
    {
      return Err(Mismatch::Kind { expected, actual: kind });
    }

    // Transaction expectations have nothing more to check; other ones (from
    // `expect_statement`) apply their matchers to transactions too.
    if self.is_transaction() {
      return Ok(());
    }

    if let Some(table) = &self.table {
      let mismatch = |actual: Option<&str>| Mismatch::Table {
        expected: table.clone(),
        actual: actual.map(str::to_string),
        backend: stmt.db_backend,
      };

      // The parsed main table when known, otherwise any mention of the table.
      match parsed.and_then(|parsed| parsed.target.as_deref()) {
        Some(target) if target != table => return Err(mismatch(Some(target))),
        Some(_) => {}
        None if !stmt.sql.contains(&quote(stmt.db_backend, table)) => return Err(mismatch(None)),
        None => {}
      }
    }

    if let Some(matcher) = self.matchers.iter().find(|matcher| !matcher.matches(stmt, parsed)) {
      return Err(Mismatch::Sql {
        matcher: Box::new(matcher.clone()),
        hint: matcher.hint(stmt, parsed),
      });
    }

    if let Some(args) = &self.args
      && let Some(wrong) = wrong_args(args, stmt.args())
    {
      return Err(Mismatch::Args {
        expected: args.clone(),
        actual: stmt.args().to_vec(),
        wrong,
      });
    }

    Ok(())
  }
}

impl Response {
  /// Whether this can answer a query: with rows, a primary key row, or an
  /// error.
  pub(crate) fn has_rows(&self) -> bool {
    self.rows.is_some() || self.returning_pk.is_some() || self.error.is_some()
  }

  /// Respond to a statement sent through `ConnectionTrait::query_*`.
  ///
  /// Rows built from `table`'s models are renamed to the aliases the statement
  /// selects (see `apply_aliases`). This runs user closures, so it must be
  /// called without holding the mock's lock.
  pub(crate) fn query(&self, table: Option<&str>, stmt: &Statement, parsed: Option<&Parsed>) -> Result<Vec<ProxyRow>, DbErr> {
    if let Some(err) = &self.error {
      return Err(err.clone());
    }

    let rows = match (&self.rows, &self.returning_pk) {
      (Some(Rows::Static(rows)), _) => rows.clone(),
      (Some(Rows::Fn(f)), _) => f(stmt)?,
      (None, Some(pk)) => vec![pk.clone()],
      (None, None) => Vec::new(),
    };

    Ok(match table {
      Some(table) if !self.raw_rows => apply_aliases(rows, table, parsed),
      _ => rows,
    })
  }

  /// Respond to a statement sent through `ConnectionTrait::execute`. This runs
  /// user closures, so it must be called without holding the mock's lock.
  pub(crate) fn exec(&self, stmt: &Statement) -> Result<ProxyExecResult, DbErr> {
    if let Some(err) = &self.error {
      return Err(err.clone());
    }

    match (&self.exec, &self.rows) {
      (Some(Exec::Static(exec)), _) => Ok(exec.clone()),
      (Some(Exec::Fn(f)), _) => f(stmt),
      (None, Some(Rows::Static(rows))) => Ok(ProxyExecResult::new(0, rows.len() as u64)),
      (None, Some(Rows::Fn(f))) => Ok(ProxyExecResult::new(0, f(stmt)?.len() as u64)),
      (None, None) => Ok(ProxyExecResult::default()),
    }
  }
}

/// A description of an expectation and of its calls, for messages: its kind,
/// its table, its SQL matchers, its arguments, then its calls when it does
/// not expect exactly one:
///
/// ```text
/// DELETE on `cake` with any SQL
/// SELECT on `cake` with SQL containing `ORDER BY` and args [Int(Some(1))] (1/2 calls)
/// any statement with SQL equal to `TRUNCATE "cake"` (optional)
/// BEGIN
/// ```
pub(crate) struct Label<'a>(&'a Expectation);

impl fmt::Display for Label<'_> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let Label(expectation) = self;
    let spec = &expectation.spec;

    match spec.kind {
      None => write!(f, "any statement")?,
      Some(kind) => write!(f, "{kind}")?,
    }

    if let Some(table) = &spec.table {
      write!(f, " on `{table}`")?;
    }

    if !spec.is_transaction() {
      write!(f, " with ")?;

      if spec.matchers.is_empty() {
        write!(f, "any SQL")?;
      }

      for (idx, matcher) in spec.matchers.iter().enumerate() {
        if idx > 0 {
          write!(f, " and ")?;
        }

        write!(f, "{matcher}")?;
      }
    }

    if let Some(args) = &spec.args {
      write!(f, " and args {args:?}")?;
    }

    match (expectation.min, expectation.max) {
      (1, 1) => {}
      (0, 1) => write!(f, " (optional)")?,
      (0, max) => write!(f, " ({}/up to {max} calls)", expectation.calls)?,
      (_, max) => write!(f, " ({}/{max} calls)", expectation.calls)?,
    }

    Ok(())
  }
}

pub(crate) fn into_proxy_row(row: impl IntoMockRow) -> ProxyRow {
  ProxyRow::new(row.into_mock_row().into_column_value_tuples().collect())
}

/// Wrap `f` into rows computed for each statement: `f` gets the statement and
/// returns models (or other rows), which are converted into the rows the mock
/// returns. Used by the `returning_with` methods.
pub(crate) fn rows_with<R: IntoMockRow>(f: impl Fn(&Statement) -> Result<Vec<R>, DbErr> + Send + Sync + 'static) -> Rows {
  Rows::Fn(Arc::new(move |stmt| Ok(f(stmt)?.into_iter().map(into_proxy_row).collect())))
}

/// Rename the columns of rows built from `table`'s models to the aliases the
/// statement selects, so that they decode as the query expects.
///
/// For example, `Entity::load()` selects every column under an alias:
///
/// ```text
/// statement   SELECT "cake"."id" AS "A_id", "cake"."name" AS "A_name" FROM "cake"
/// row         id = 1, name = "Chocolate"
/// becomes     A_id = 1, A_name = "Chocolate"
/// ```
///
/// Each alias takes its value from the original row, so that a column can be
/// selected under several aliases, or under the name of another column (`id AS
/// name`). The original column is kept when the statement also selects it under
/// its own name, or through a wildcard. Aliases come from the parsed statement
/// (`Parsed::aliases`): rows of a statement that does not parse are left as
/// they are.
fn apply_aliases(mut rows: Vec<ProxyRow>, table: &str, parsed: Option<&Parsed>) -> Vec<ProxyRow> {
  let Some(parsed) = parsed else {
    return rows;
  };

  let aliases = parsed
    .aliases
    .iter()
    .filter(|(t, column, alias)| t == table && column != alias)
    .map(|(_, column, alias)| (column, alias))
    .collect::<Vec<_>>();

  if aliases.is_empty() {
    return rows;
  }

  let kept = |column: &str| parsed.wildcard || parsed.plain.iter().any(|(t, c)| t == table && c == column) || aliases.iter().any(|(_, alias)| *alias == column);

  for row in &mut rows {
    let original = row.values.clone();

    for (column, alias) in &aliases {
      if let Some(value) = original.get(*column) {
        row.values.insert(alias.to_string(), value.clone());
      }
    }

    for (column, _) in &aliases {
      if !kept(column) {
        row.values.remove(*column);
      }
    }
  }

  rows
}

/// Describe a statement for plain-text messages: its SQL, with whitespace
/// collapsed (see `normalize`), and its values:
/// ``"`SELECT "id" FROM "cake" WHERE "id" = $1` with [Int(Some(1))]"``.
pub(crate) fn describe(stmt: &Statement) -> String {
  format!("`{}` with {:?}", normalize(&stmt.sql, stmt.db_backend), stmt.args())
}

/// Quote an identifier as `backend` does in SQL: `cake` gives `"cake"`, or
/// `` `cake` `` on MySQL.
pub(crate) fn quote(backend: DbBackend, ident: &str) -> String {
  match backend {
    DbBackend::MySql => format!("`{ident}`"),
    _ => format!("\"{ident}\""),
  }
}
