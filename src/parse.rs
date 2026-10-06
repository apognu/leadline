use std::ops::Range;

use sea_orm::{DbBackend, Statement, Value};
use sqlparser::{
  ast::{self, Expr, LimitClause, ObjectName, ObjectNamePart, SelectItem, SetExpr, TableFactor, TableObject},
  dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect},
  parser::Parser,
  tokenizer::{Token, Tokenizer},
};

use crate::{classify::StmtKind, matcher::StatementExt};

/// What leadline needs to know about a statement, read from its parsed SQL.
pub(crate) struct Parsed {
  /// The kind of the statement. For `WITH old AS (…) DELETE …`, it is the kind
  /// of the body: `DELETE`.
  pub kind: StmtKind,
  /// The table the statement is on: `t` in `SELECT … FROM t`, `INSERT INTO t`,
  /// `UPDATE t` and `DELETE FROM t`. Subqueries and common table expressions
  /// are followed: `WITH c AS (SELECT … FROM cake) SELECT … FROM c` is on
  /// `cake`. A schema is ignored: `"org"."accounts"` is `accounts`.
  pub target: Option<String>,
  /// The columns selected under an alias, as `(table, column, alias)`: `SELECT
  /// "c"."id" AS "A_id" FROM "cake" AS "c"` gives `("cake", "id", "A_id")`.
  /// Casts are followed: `CAST("t"."kind" AS text) AS "k"` gives `("t", "kind",
  /// "k")`.
  pub aliases: Vec<(String, String, String)>,
  /// The columns selected under their own name, as `(table, column)`: `SELECT
  /// "c"."id" FROM "cake" AS "c"` gives `("cake", "id")`.
  pub plain: Vec<(String, String)>,
  /// Whether the projection selects a wildcard (`*` or `t.*`).
  pub wildcard: bool,
  /// The statement without its `LIMIT` and `OFFSET`, and its bound values
  /// without theirs: `SELECT … WHERE "id" = $1 LIMIT $2` with `[1, 1]` gives
  /// `SELECT … WHERE "id" = $1` with `[1]`. The SQL is rendered by sqlparser,
  /// so that two statements compare equal whatever their formatting.
  pub unpaginated: (String, Vec<Value>),
}

/// Parse `stmt` with the dialect of its backend. Returns `None` for anything
/// that does not parse as a single statement, so that callers can fall back
/// to heuristics on the SQL text.
pub(crate) fn parse(stmt: &Statement) -> Option<Parsed> {
  let mut statements = Parser::parse_sql(dialect(stmt.db_backend).as_ref(), &stmt.sql).ok()?;

  if statements.len() != 1 {
    return None;
  }

  let ast = statements.pop()?;
  let ctes = ctes(&ast);
  let projection = projection(&ast, &ctes);

  Some(Parsed {
    kind: statement_kind(&ast),
    target: statement_target(&ast, &ctes, &[]),
    aliases: projection.aliases,
    plain: projection.plain,
    wildcard: projection.wildcard,
    unpaginated: unpaginated(ast, stmt.args()),
  })
}

/// The SQL dialect of a backend.
pub(crate) fn dialect(backend: DbBackend) -> Box<dyn Dialect> {
  match backend {
    DbBackend::Postgres => Box::new(PostgreSqlDialect {}),
    DbBackend::MySql => Box::new(MySqlDialect {}),
    DbBackend::Sqlite => Box::new(SQLiteDialect {}),
    #[allow(unreachable_patterns)]
    _ => Box::new(GenericDialect {}),
  }
}

/// Split `sql` into tokens, in the dialect of `backend`, each with its byte
/// range in `sql`: `SELECT  "id"` gives `SELECT` at `0..6` and `"id"` at
/// `8..12`, with whitespace tokens in between.
///
/// A range covers its token without the whitespace after it: whitespace
/// tokens get empty ranges, and a `--` comment, which the tokenizer ends with
/// its line break, gets a range without it. Returns
/// `None` when `sql` does not tokenize, such as a fragment ending inside a
/// quote.
pub(crate) fn tokens(sql: &str, backend: DbBackend) -> Option<Vec<(Token, Range<usize>)>> {
  let tokens: Vec<_> = Tokenizer::new(dialect(backend).as_ref(), sql)
    .tokenize_with_location()
    .ok()?
    .into_iter()
    .filter(|token| token.token != Token::EOF)
    .collect();

  let offsets = token_offsets(sql, tokens.iter().map(|token| (token.span.start.line, token.span.start.column)));

  let tokens = tokens
    .into_iter()
    .enumerate()
    .map(|(index, token)| {
      // Tokens are contiguous: each one runs until the next one starts.
      let end = offsets.get(index + 1).copied().unwrap_or(sql.len());
      let text = sql[offsets[index]..end].trim_end();

      (token.token, offsets[index]..offsets[index] + text.len())
    })
    .collect();

  Some(tokens)
}

/// Convert the positions of tokens, which the tokenizer gives as a line and a
/// column (both counted from 1, in characters), into byte offsets in `sql`.
/// In `"SELECT\n  1"`, line 2, column 3 (the `1`) is at byte 9. Positions must
/// come in order: the text is scanned once.
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

fn statement_kind(ast: &ast::Statement) -> StmtKind {
  match ast {
    ast::Statement::Query(query) => set_expr_kind(&query.body),
    ast::Statement::Insert(_) => StmtKind::Insert,
    ast::Statement::Update(_) => StmtKind::Update,
    ast::Statement::Delete(_) => StmtKind::Delete,
    ast::Statement::StartTransaction { .. } => StmtKind::Begin,
    ast::Statement::Commit { .. } => StmtKind::Commit,
    ast::Statement::Rollback { .. } => StmtKind::Rollback,
    _ => StmtKind::Other,
  }
}

fn set_expr_kind(body: &SetExpr) -> StmtKind {
  match body {
    SetExpr::Select(_) | SetExpr::SetOperation { .. } | SetExpr::Values(_) | SetExpr::Table(_) => StmtKind::Select,
    SetExpr::Query(query) => set_expr_kind(&query.body),
    SetExpr::Insert(_) => StmtKind::Insert,
    SetExpr::Update(_) => StmtKind::Update,
    SetExpr::Delete(_) => StmtKind::Delete,
    _ => StmtKind::Other,
  }
}

/// Collect the common table expressions of `ast`, by name.
fn ctes(ast: &ast::Statement) -> Vec<(String, &ast::Query)> {
  let ast::Statement::Query(query) = ast else {
    return Vec::new();
  };

  query
    .with
    .iter()
    .flat_map(|with| &with.cte_tables)
    .map(|cte| (cte.alias.name.value.clone(), cte.query.as_ref()))
    .collect()
}

/// The common table expressions of a statement, by name.
type Ctes<'a> = [(String, &'a ast::Query)];

/// The table the statement is on (see `Parsed::target`). `resolving` holds the
/// names of the CTEs being resolved, to stop on recursive or cyclic ones.
fn statement_target(ast: &ast::Statement, ctes: &Ctes, resolving: &[&str]) -> Option<String> {
  match ast {
    ast::Statement::Query(query) => set_expr_target(&query.body, ctes, resolving),
    ast::Statement::Insert(insert) => match &insert.table {
      TableObject::TableName(name) => table_name(name),
      _ => None,
    },
    ast::Statement::Update(update) => factor_target(&update.table.relation, ctes, resolving),
    ast::Statement::Delete(delete) => match &delete.from {
      ast::FromTable::WithFromKeyword(tables) | ast::FromTable::WithoutKeyword(tables) => tables.first().and_then(|table| factor_target(&table.relation, ctes, resolving)),
    },
    _ => None,
  }
}

fn set_expr_target(body: &SetExpr, ctes: &Ctes, resolving: &[&str]) -> Option<String> {
  match body {
    SetExpr::Select(select) => select.from.first().and_then(|table| factor_target(&table.relation, ctes, resolving)),
    SetExpr::Query(query) => set_expr_target(&query.body, ctes, resolving),
    SetExpr::SetOperation { left, .. } => set_expr_target(left, ctes, resolving),
    SetExpr::Insert(ast) | SetExpr::Update(ast) | SetExpr::Delete(ast) => statement_target(ast, ctes, resolving),
    _ => None,
  }
}

fn factor_target(factor: &TableFactor, ctes: &Ctes, resolving: &[&str]) -> Option<String> {
  match factor {
    TableFactor::Table { name, .. } => {
      let table = table_name(name)?;

      // A common table expression stands for the table its query reads,
      // possibly through other CTEs. A CTE reading itself (recursive, or in
      // a cycle) has no table of its own.
      match ctes.iter().find(|(cte, _)| *cte == table) {
        Some(_) if resolving.contains(&table.as_str()) => None,
        Some((cte, query)) => {
          let resolving = [resolving, &[cte.as_str()]].concat();

          set_expr_target(&query.body, ctes, &resolving)
        }
        None => Some(table),
      }
    }
    TableFactor::Derived { subquery, .. } => set_expr_target(&subquery.body, ctes, resolving),
    _ => None,
  }
}

/// The table of a name, without its schema: `"org"."accounts"` gives
/// `accounts`.
fn table_name(name: &ObjectName) -> Option<String> {
  match name.0.last()? {
    ObjectNamePart::Identifier(ident) => Some(ident.value.clone()),
    _ => None,
  }
}

#[derive(Default)]
struct Projection {
  aliases: Vec<(String, String, String)>,
  plain: Vec<(String, String)>,
  wildcard: bool,
}

/// The columns a query selects, with and without aliases (see `Parsed::aliases`
/// and `Parsed::plain`). For a set operation such as `UNION`, the columns of
/// its first `SELECT`, which name the columns of the result.
fn projection(ast: &ast::Statement, ctes: &Ctes) -> Projection {
  let ast::Statement::Query(query) = ast else {
    return Projection::default();
  };

  // Columns are named by the first SELECT of a set operation.
  let mut body = query.body.as_ref();

  let select = loop {
    match body {
      SetExpr::Select(select) => break select,
      SetExpr::Query(query) => body = &query.body,
      SetExpr::SetOperation { left, .. } => body = left,
      _ => return Projection::default(),
    }
  };

  let tables = from_tables(select, ctes);

  // Unqualified columns belong to the only table read, if there is one.
  let only_table = match tables.as_slice() {
    [(_, table)] => Some(table.clone()),
    _ => None,
  };

  // A table alias stands for its table; other qualifiers (e.g. a common table
  // expression) are kept as they are.
  let resolve = |qualifier: Option<String>| match qualifier {
    Some(qualifier) => Some(tables.iter().find(|(name, _)| *name == qualifier).map_or(qualifier, |(_, table)| table.clone())),
    None => only_table.clone(),
  };

  let mut projection = Projection::default();

  for item in &select.projection {
    match item {
      SelectItem::ExprWithAlias { expr, alias } => {
        if let Some((qualifier, column)) = column(expr)
          && let Some(table) = resolve(qualifier)
        {
          projection.aliases.push((table, column, alias.value.clone()));
        }
      }
      SelectItem::UnnamedExpr(expr) => {
        if let Some((qualifier, column)) = column(expr)
          && let Some(table) = resolve(qualifier)
        {
          projection.plain.push((table, column));
        }
      }
      SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => projection.wildcard = true,
      _ => {}
    }
  }

  projection
}

/// The tables a `SELECT` reads, joins included, as `(name in the query,
/// table)`:
///
/// - `FROM "cake"` gives `("cake", "cake")`;
/// - `FROM "cake" AS "c"` gives `("c", "cake")`;
/// - `WITH "c" AS (SELECT … FROM "cake") … FROM "c"` gives `("c", "cake")`, and
///   so does the subquery `FROM (SELECT … FROM "cake") AS "c"`.
fn from_tables(select: &ast::Select, ctes: &Ctes) -> Vec<(String, String)> {
  select
    .from
    .iter()
    .flat_map(|table| std::iter::once(&table.relation).chain(table.joins.iter().map(|join| &join.relation)))
    .filter_map(|factor| {
      let table = factor_target(factor, ctes, &[])?;

      let name = match factor {
        TableFactor::Table { name, alias, .. } => match alias {
          Some(alias) => alias.name.value.clone(),
          None => table_name(name)?,
        },
        TableFactor::Derived { alias: Some(alias), .. } => alias.name.value.clone(),
        _ => return None,
      };

      Some((name, table))
    })
    .collect()
}

/// The `(qualifier, column)` an expression reads, through casts and
/// parentheses: `"c"."id"` gives `(Some("c"), "id")`, `"id"` gives `(None,
/// "id")`.
fn column(expr: &Expr) -> Option<(Option<String>, String)> {
  match expr {
    Expr::CompoundIdentifier(idents) if idents.len() >= 2 => {
      let [.., qualifier, column] = idents.as_slice() else {
        return None;
      };

      Some((Some(qualifier.value.clone()), column.value.clone()))
    }
    Expr::Identifier(column) => Some((None, column.value.clone())),
    Expr::Cast { expr, .. } | Expr::Nested(expr) => column(expr),
    _ => None,
  }
}

/// Remove the `LIMIT` and `OFFSET` clause of a query, and the values bound to
/// them (see `Parsed::unpaginated`).
///
/// Postgres numbers its placeholders, so `LIMIT $2` drops the value at index 1.
/// MySQL and SQLite use `?`, which carries no position: the clause's values are
/// then the last ones, as the clause ends the statement (only a row lock, such
/// as `FOR UPDATE`, which binds nothing, can follow it).
fn unpaginated(mut ast: ast::Statement, values: &[Value]) -> (String, Vec<Value>) {
  let mut dropped = Vec::new();

  if let ast::Statement::Query(query) = &mut ast
    && let Some(limit) = query.limit_clause.take()
  {
    let exprs: Vec<&Expr> = match &limit {
      LimitClause::LimitOffset { limit, offset, .. } => limit.iter().chain(offset.iter().map(|offset| &offset.value)).collect(),
      LimitClause::OffsetCommaLimit { offset, limit } => vec![offset, limit],
    };

    let placeholders: Vec<&str> = exprs.into_iter().filter_map(placeholder).collect();

    for (position, placeholder) in placeholders.iter().enumerate() {
      let index = match placeholder.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) {
        // Numbered placeholders (Postgres) name their value.
        Some(number) => number.checked_sub(1),
        // Positional placeholders bind the clause's values last, as it ends
        // the statement (only row locks may follow, without values).
        None => (values.len() + position).checked_sub(placeholders.len()),
      };

      dropped.extend(index);
    }
  }

  let values = values.iter().enumerate().filter(|(index, _)| !dropped.contains(index)).map(|(_, value)| value.clone()).collect();

  (ast.to_string(), values)
}

/// The placeholder an expression is, if any: `$2` or `?`.
fn placeholder(expr: &Expr) -> Option<&str> {
  match expr {
    Expr::Value(value) => match &value.value {
      ast::Value::Placeholder(placeholder) => Some(placeholder),
      _ => None,
    },
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse_sql(backend: DbBackend, sql: &str, values: Vec<Value>) -> Parsed {
    parse(&Statement::from_sql_and_values(backend, sql, values)).expect("statement should parse")
  }

  #[test]
  fn kinds() {
    let kind = |sql| parse_sql(DbBackend::Postgres, sql, vec![]).kind;

    assert_eq!(kind(r#"SELECT "id" FROM "cake""#), StmtKind::Select);
    assert_eq!(kind(r#"(SELECT 1) UNION (SELECT 2)"#), StmtKind::Select);
    assert_eq!(kind(r#"INSERT INTO "cake" ("name") VALUES ($1)"#), StmtKind::Insert);
    assert_eq!(kind(r#"UPDATE "cake" SET "name" = $1"#), StmtKind::Update);
    assert_eq!(kind(r#"DELETE FROM "cake""#), StmtKind::Delete);
    assert_eq!(kind("BEGIN"), StmtKind::Begin);
    assert_eq!(kind("COMMIT"), StmtKind::Commit);
    assert_eq!(kind("ROLLBACK"), StmtKind::Rollback);
    assert_eq!(kind(r#"TRUNCATE "cake""#), StmtKind::Other);
  }

  #[test]
  fn kind_of_a_data_modifying_cte_is_its_body() {
    let parsed = parse_sql(
      DbBackend::Postgres,
      r#"WITH "old" AS (SELECT "id" FROM "cake" WHERE "stale") DELETE FROM "cake" WHERE "id" IN (SELECT "id" FROM "old")"#,
      vec![],
    );

    assert_eq!(parsed.kind, StmtKind::Delete);
    assert_eq!(parsed.target.as_deref(), Some("cake"));
  }

  #[test]
  fn targets() {
    let target = |backend, sql| parse_sql(backend, sql, vec![]).target;

    assert_eq!(
      target(DbBackend::Postgres, r#"SELECT "bakery"."id" FROM "bakery" INNER JOIN "cake" ON "cake"."bakery_id" = "bakery"."id""#).as_deref(),
      Some("bakery")
    );
    assert_eq!(target(DbBackend::Postgres, r#"SELECT "x"."id" FROM (SELECT "id" FROM "cake") AS "x""#).as_deref(), Some("cake"));
    assert_eq!(
      target(DbBackend::Postgres, r#"SELECT "table_1"."name" FROM "org-acme"."accounts" AS "table_1""#).as_deref(),
      Some("accounts")
    );
    assert_eq!(target(DbBackend::Postgres, r#"WITH "c" AS (SELECT * FROM "cake") SELECT * FROM "c""#).as_deref(), Some("cake"));
    assert_eq!(target(DbBackend::MySql, "INSERT INTO `cake` (`name`) VALUES (?)").as_deref(), Some("cake"));
    assert_eq!(target(DbBackend::Sqlite, r#"UPDATE "cake" SET "name" = ?"#).as_deref(), Some("cake"));
    assert_eq!(target(DbBackend::Postgres, "SELECT 1").as_deref(), None);
  }

  #[test]
  fn targets_through_chained_ctes() {
    let target = |sql| parse_sql(DbBackend::Postgres, sql, vec![]).target;

    assert_eq!(
      target(r#"WITH "a" AS (SELECT "id" FROM "cake"), "b" AS (SELECT "id" FROM "a") SELECT "id" FROM "b""#).as_deref(),
      Some("cake")
    );

    // A recursive CTE has no table of its own, and must not loop.
    assert_eq!(target(r#"WITH RECURSIVE "t" AS (SELECT "id" FROM "t") SELECT "id" FROM "t""#), None);
    assert_eq!(target(r#"WITH "a" AS (SELECT * FROM "b"), "b" AS (SELECT * FROM "a") SELECT * FROM "a""#), None);
  }

  #[test]
  fn aliases_see_through_casts() {
    let parsed = parse_sql(
      DbBackend::Postgres,
      r#"SELECT "ticket"."id" AS "A_id", CAST("ticket"."status" AS "text") AS "A_status", CAST("ticket"."status" AS "text") FROM "ticket""#,
      vec![],
    );

    assert_eq!(parsed.aliases, [("ticket".into(), "id".into(), "A_id".into()), ("ticket".into(), "status".into(), "A_status".into())]);
  }

  #[test]
  fn aliases_resolve_table_aliases() {
    let parsed = parse_sql(
      DbBackend::Postgres,
      r#"SELECT "c"."id" AS "x_id", "b"."name" AS "x_bakery" FROM "cake" AS "c" LEFT JOIN "bakery" AS "b" ON "b"."id" = "c"."bakery_id""#,
      vec![],
    );

    assert_eq!(parsed.aliases, [("cake".into(), "id".into(), "x_id".into()), ("bakery".into(), "name".into(), "x_bakery".into())]);
  }

  #[test]
  fn aliases_through_ctes_and_subqueries() {
    let cte = parse_sql(DbBackend::Postgres, r#"WITH "c" AS (SELECT "id" FROM "cake") SELECT "c"."id" AS "value" FROM "c""#, vec![]);
    assert_eq!(cte.aliases, [("cake".into(), "id".into(), "value".into())]);

    let subquery = parse_sql(DbBackend::Postgres, r#"SELECT "s"."id" AS "value" FROM (SELECT "id" FROM "cake") AS "s""#, vec![]);
    assert_eq!(subquery.aliases, [("cake".into(), "id".into(), "value".into())]);

    let unqualified = parse_sql(DbBackend::Postgres, r#"WITH "c" AS (SELECT "id" FROM "cake") SELECT "id" AS "value" FROM "c""#, vec![]);
    assert_eq!(unqualified.aliases, [("cake".into(), "id".into(), "value".into())]);
  }

  #[test]
  fn aliases_of_unqualified_columns() {
    let single = parse_sql(DbBackend::MySql, "SELECT `id` AS `x_id` FROM `cake`", vec![]);
    assert_eq!(single.aliases, [("cake".into(), "id".into(), "x_id".into())]);

    // With several tables, an unqualified column cannot be attributed.
    let joined = parse_sql(DbBackend::Postgres, r#"SELECT "name" AS "x_name" FROM "cake" JOIN "bakery" ON TRUE"#, vec![]);
    assert!(joined.aliases.is_empty());
  }

  #[test]
  fn aliases_of_a_set_operation_come_from_its_first_select() {
    let parsed = parse_sql(DbBackend::Postgres, r#"SELECT "cake"."id" AS "x" FROM "cake" UNION SELECT "bakery"."id" AS "y" FROM "bakery""#, vec![]);

    assert_eq!(parsed.aliases, [("cake".into(), "id".into(), "x".into())]);
  }

  #[test]
  fn plain_columns_and_wildcards() {
    let parsed = parse_sql(DbBackend::Postgres, r#"SELECT "c"."id", "c"."id" AS "other", "name" FROM "cake" AS "c""#, vec![]);
    assert_eq!(parsed.plain, [("cake".into(), "id".into()), ("cake".into(), "name".into())]);
    assert_eq!(parsed.aliases, [("cake".into(), "id".into(), "other".into())]);
    assert!(!parsed.wildcard);

    assert!(parse_sql(DbBackend::Postgres, r#"SELECT *, "id" AS "x" FROM "cake""#, vec![]).wildcard);
    assert!(parse_sql(DbBackend::Postgres, r#"SELECT "c".*, "c"."id" AS "x" FROM "cake" AS "c""#, vec![]).wildcard);
  }

  #[test]
  fn pagination_is_removed_with_its_values() {
    let postgres = parse_sql(
      DbBackend::Postgres,
      r#"SELECT * FROM "cake" WHERE "id" = $1 LIMIT $2 OFFSET $3 FOR UPDATE"#,
      vec![1.into(), 10u64.into(), 20u64.into()],
    );
    assert_eq!(postgres.unpaginated, (r#"SELECT * FROM "cake" WHERE "id" = $1 FOR UPDATE"#.to_string(), vec![1.into()]));

    let mysql = parse_sql(DbBackend::MySql, "SELECT * FROM `cake` WHERE `id` = ? LIMIT ?, ?", vec![1.into(), 20u64.into(), 10u64.into()]);
    assert_eq!(mysql.unpaginated, ("SELECT * FROM `cake` WHERE `id` = ?".to_string(), vec![1.into()]));

    let literal = parse_sql(DbBackend::Sqlite, r#"SELECT * FROM "cake" WHERE "id" = ? LIMIT 1"#, vec![1.into()]);
    assert_eq!(literal.unpaginated, (r#"SELECT * FROM "cake" WHERE "id" = ?"#.to_string(), vec![1.into()]));
  }

  #[test]
  fn unparseable_sql_is_none() {
    assert!(parse(&Statement::from_string(DbBackend::Postgres, "NOT SQL AT ALL")).is_none());
    assert!(parse(&Statement::from_string(DbBackend::Postgres, "SELECT 1; SELECT 2")).is_none());
  }
}
