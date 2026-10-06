use std::ops::Range;

use sea_orm::{
  DbBackend, Value,
  sea_query::{MysqlQueryBuilder, PostgresQueryBuilder, QueryBuilder, SqliteQueryBuilder},
};
use similar::{Algorithm, DiffOp, capture_diff_slices};
use sqlparser::tokenizer::{Token, Whitespace};

use crate::{matcher::Arg, parse};

/// One difference between an expected text and an actual one: in the expected
/// text, the bytes in `expected` are replaced by `replacement`.
///
/// With the expected text `[7, 1]` and the actual text `[8, 1]`, the change is
/// `1..2` replaced by `"8"`. An empty range is an insertion at that offset,
/// and an empty replacement is a deletion.
///
/// For two statements (see `sql`), applying every change to the expected text
/// gives the actual text. For two lists of arguments (see `args`), it may not:
/// an argument that accepts a value it does not read like, such as `_`
/// against `'Lemon'`, is left as it is.
#[derive(Debug, PartialEq)]
pub(crate) struct Change {
  pub expected: Range<usize>,
  pub replacement: String,
}

/// Compare two SQL statements token by token, and return the changes that
/// turn the expected one into the actual one:
///
/// ```text
/// expected   SELECT "id" FROM "cake" WHERE "id" = $1
/// actual     SELECT "id" FROM "cake" WHERE "id" = $1 LIMIT $2
/// changes    insert " LIMIT $2" at the end
/// ```
///
/// Whitespace is ignored: `SELECT "id"` against `SELECT\n  "id"` gives no
/// change. Comments are not ignored, as matching does not ignore them either.
///
/// Returns `None` when either statement does not tokenize (for example, a
/// quote that is never closed), and no changes when they are the same.
pub(crate) fn sql(expected: &str, actual: &str, backend: DbBackend) -> Option<Vec<Change>> {
  let expected_tokens = tokens(expected, backend)?;
  let actual_tokens = tokens(actual, backend)?;

  let expected_text: Vec<&str> = expected_tokens.iter().map(|range| &expected[range.clone()]).collect();
  let actual_text: Vec<&str> = actual_tokens.iter().map(|range| &actual[range.clone()]).collect();

  let ops = capture_diff_slices(Algorithm::Myers, &expected_text, &actual_text);

  Some(changes(&expected_tokens, actual, &actual_tokens, 0, ops))
}

/// The byte ranges of the tokens of `sql`, whitespace excluded: for
/// `SELECT "id"`, `0..6` and `7..11`.
fn tokens(sql: &str, backend: DbBackend) -> Option<Vec<Range<usize>>> {
  let tokens = parse::tokens(sql, backend)?;

  let ranges = tokens
    .into_iter()
    .filter(|(token, _)| !matches!(token, Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab)))
    .map(|(_, range)| range)
    .collect();

  Some(ranges)
}

/// Turn diff operations into changes to the expected text.
///
/// The operations come from comparing two lists of items: the tokens of two
/// statements, or the values in two lists such as `[1, 2]`. `expected` and
/// `actual` give the byte range of each item in its text. `start` is where
/// the first item may start in both texts: `0` for statements, and `1` for
/// lists, after their `[`.
///
/// An item added or removed takes the separator after it along (a space
/// between tokens, `, ` in a list), or the one before it when it is the
/// last item. So applying the changes reads naturally: inserting `LIMIT $2`
/// after `… = $1` gives `… = $1 LIMIT $2`, not `… = $1LIMIT $2`, and removing
/// `2` from `[1, 2]` gives `[1]`, not `[1, ]`.
fn changes(expected: &[Range<usize>], actual_text: &str, actual: &[Range<usize>], start: usize, ops: Vec<DiffOp>) -> Vec<Change> {
  // The byte range of `len` items from `index`, with the separator after
  // them, or the one before them when they are the last ones: in `[1, 2, 3]`,
  // item 1 (`2`) gives `2, `, and item 2 (`3`) gives `, 3`.
  let run = |items: &[Range<usize>], index: usize, len: usize| {
    let end = items[index + len - 1].end;

    match (items.get(index + len), index.checked_sub(1)) {
      (Some(next), _) => items[index].start..next.start,
      (None, Some(previous)) => items[previous].end..end,
      (None, None) => start..end,
    }
  };

  ops
    .into_iter()
    .filter_map(|op| match op {
      DiffOp::Equal { .. } => None,
      DiffOp::Delete { old_index, old_len, .. } => Some(Change {
        expected: run(expected, old_index, old_len),
        replacement: String::new(),
      }),
      DiffOp::Insert { old_index, new_index, new_len } => {
        // Before the next item, or at the end.
        let at = match expected.get(old_index) {
          Some(next) => next.start,
          None => old_index.checked_sub(1).map_or(start, |previous| expected[previous].end),
        };

        Some(Change {
          expected: at..at,
          replacement: actual_text[run(actual, new_index, new_len)].to_string(),
        })
      }
      DiffOp::Replace {
        old_index,
        old_len,
        new_index,
        new_len,
      } => Some(Change {
        expected: expected[old_index].start..expected[old_index + old_len - 1].end,
        replacement: actual_text[actual[new_index].start..actual[new_index + new_len - 1].end].to_string(),
      }),
    })
    .collect()
}

/// Render the arguments of `with_args` and the values bound to a statement as
/// two lists, and return them with the changes to the first list that show
/// where the values do not match:
///
/// ```text
/// expected   [7, _]           from with_args((7, Any))
/// actual     [8, 'Lemon']
/// changes    replace `7` by `8`
/// ```
///
/// How the changes are found depends on the number of values:
///
/// - With as many values as arguments, `wrong` gives the positions of the
///   values that do not match their argument, `[0]` above, and there is one
///   change for each of them: the argument at that position is replaced by
///   the value at the same position, `7` by `8` above. The other positions
///   get no change, even when their texts differ, as `_` and `'Lemon'` above.
///
///   The caller gets `wrong` from `wrong_args`, which ran when the statement
///   was matched, and applied the rules of `with_args`. Comparing the
///   rendered texts here would not apply them: `_` reads unlike `'Lemon'`,
///   but `Any` accepts it, and `<predicate>` reads unlike any value, though
///   its closure may accept it. Calling the closure here instead would run
///   the user's code a second time.
/// - With more or fewer values than arguments, there are no positions to
///   compare one by one, and `wrong` is ignored. The two lists are compared
///   as text, to find the values in excess or missing: `[1, 2]` against `[1]`
///   removes `, 2`. Here, `_` and `<predicate>` are compared as text too, so
///   they show as changed even where they would accept the value; the note
///   counting the arguments and the values gives the actual problem.
pub(crate) fn args(expected: &[Arg], actual: &[Value], wrong: &[usize], backend: DbBackend) -> (String, String, Vec<Change>) {
  let expected_items: Vec<String> = expected.iter().map(|arg| display_arg(arg, backend)).collect();
  let actual_items: Vec<String> = actual.iter().map(|value| literal(value, backend)).collect();

  let (expected_list, expected_ranges) = list(&expected_items);
  let (actual_list, actual_ranges) = list(&actual_items);

  let ops = if expected.len() == actual.len() {
    wrong
      .iter()
      .map(|&index| DiffOp::Replace {
        old_index: index,
        old_len: 1,
        new_index: index,
        new_len: 1,
      })
      .collect()
  } else {
    capture_diff_slices(Algorithm::Myers, &expected_items, &actual_items)
  };

  // Items start after the opening bracket.
  let changes = changes(&expected_ranges, &actual_list, &actual_ranges, 1, ops);

  (expected_list, actual_list, changes)
}

/// Render `items` as a list, and return it with the byte range of each item
/// in it: `["1", "2"]` gives `[1, 2]`, with the ranges `1..2` and `4..5`.
fn list(items: &[String]) -> (String, Vec<Range<usize>>) {
  let mut list = String::from("[");
  let mut ranges = Vec::with_capacity(items.len());

  for (index, item) in items.iter().enumerate() {
    if index > 0 {
      list.push_str(", ");
    }

    ranges.push(list.len()..list.len() + item.len());
    list.push_str(item);
  }

  list.push(']');

  (list, ranges)
}

/// An argument of `with_args`, as failures show it: a plain value as a SQL
/// literal (`7`, `'Lemon'`), `_` for `Any`, and `<predicate>` for
/// `Arg::matching`, whose closure cannot be shown.
pub(crate) fn display_arg(arg: &Arg, backend: DbBackend) -> String {
  match arg.value() {
    Some(value) => literal(value, backend),
    None if arg.is_any() => "_".to_string(),
    None => "<predicate>".to_string(),
  }
}

/// A bound value, written as a SQL literal of `backend`: `1`, `'Lemon'`,
/// `NULL`, or `E'it\'s'` for `it's` on Postgres.
pub(crate) fn literal(value: &Value, backend: DbBackend) -> String {
  match backend {
    DbBackend::MySql => MysqlQueryBuilder.value_to_string(value),
    DbBackend::Sqlite => SqliteQueryBuilder.value_to_string(value),
    _ => PostgresQueryBuilder.value_to_string(value),
  }
}

/// A statement with its bound values written in place of their placeholders
/// (see `interpolate`).
#[derive(Debug, PartialEq)]
pub(crate) struct Interpolated {
  pub text: String,
  /// Where each value was written in `text`: the index of the value, and the
  /// byte range of the value itself, without what surrounds it. A value whose
  /// placeholder appears twice, as `$1` in `SELECT $1, $1`, is listed twice.
  pub values: Vec<(usize, Range<usize>)>,
}

/// Write `values` (already SQL literals) in place of the placeholders of
/// `sql`, each one between the two strings of `around`:
///
/// ```text
/// sql      SELECT "id" FROM "cake" WHERE "name" = $1 LIMIT $2
/// values   'Lemon', 1
/// around   ("{", "}")
/// result   SELECT "id" FROM "cake" WHERE "name" = {'Lemon'} LIMIT {1}
/// ```
///
/// What surrounds the values tells them from literals written in the SQL:
/// `"id" = {7}` is a bound value, `"id" = 7` is not. Diagnostics use braces
/// without colors, and invisible markers that become colors otherwise.
///
/// `$n` (Postgres) and `?n` (SQLite) take the n-th value, counting from 1.
/// Plain `?` placeholders (MySQL, SQLite) take the values in order: the first
/// `?` the first value, the second `?` the second value, and so on.
/// Placeholders inside strings or comments are left alone.
///
/// Returns `None` when the placeholders and the values do not line up: a
/// placeholder has no value, or a value has no placeholder. Also when the
/// statement does not tokenize, or uses named placeholders (`:id`, `@id`),
/// which SeaORM does not send.
pub(crate) fn interpolate(sql: &str, values: &[String], backend: DbBackend, around: (&str, &str)) -> Option<Interpolated> {
  let mut text = String::with_capacity(sql.len());
  let mut positions = Vec::new();
  let mut used = vec![false; values.len()];
  let mut next = 0;
  let mut copied = 0;

  for (token, range) in parse::tokens(sql, backend)? {
    let Token::Placeholder(placeholder) = token else {
      continue;
    };

    let index = match placeholder.strip_prefix(['$', '?']) {
      Some("") => {
        next += 1;
        next - 1
      }
      Some(n) => n.parse::<usize>().ok()?.checked_sub(1)?,
      None => return None,
    };

    let value = values.get(index)?;
    used[index] = true;

    text.push_str(&sql[copied..range.start]);
    text.push_str(around.0);
    positions.push((index, text.len()..text.len() + value.len()));
    text.push_str(value);
    text.push_str(around.1);

    copied = range.end;
  }

  if used.contains(&false) {
    return None;
  }

  text.push_str(&sql[copied..]);

  Some(Interpolated { text, values: positions })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::matcher::{Any, sealed::IntoArgs as _};

  /// `expected` with `changes` applied, which must give the actual text.
  fn apply(expected: &str, changes: &[Change]) -> String {
    let mut text = expected.to_string();

    for change in changes.iter().rev() {
      text.replace_range(change.expected.clone(), &change.replacement);
    }

    text
  }

  #[test]
  fn sql_changes() {
    let expected = r#"SELECT "id" FROM "cake" WHERE "id" = $1"#;

    let actual = r#"SELECT "id" FROM "cake" WHERE "id" = $1 LIMIT $2"#;
    let changes = sql(expected, actual, DbBackend::Postgres).unwrap();
    assert_eq!(
      changes,
      [Change {
        expected: expected.len()..expected.len(),
        replacement: " LIMIT $2".into()
      }]
    );

    for actual in [
      r#"SELECT "name" FROM "cake" WHERE "id" = $1"#,
      r#"SELECT "id" FROM "cake""#,
      r#"WITH x AS (SELECT 1) SELECT "id" FROM "cake" WHERE "id" = $1"#,
      r#""id" FROM "cake" WHERE "id" = $1"#,
      r#"SELECT "id" FROM "cake" WHERE "id" = {$1}"#,
      r#"SELECT "id" FROM "cake" LIMIT 1 WHERE "id" = $1"#,
    ] {
      let changes = sql(expected, actual, DbBackend::Postgres).unwrap();
      assert_eq!(apply(expected, &changes), actual);
    }

    // Formatting is not a difference.
    let changes = sql(expected, "SELECT  \"id\"\nFROM \"cake\" WHERE \"id\" = $1", DbBackend::Postgres).unwrap();
    assert!(changes.is_empty());
  }

  #[test]
  fn sql_that_does_not_tokenize() {
    assert_eq!(sql("SELECT 'unterminated", "SELECT 1", DbBackend::Postgres), None);
  }

  #[test]
  fn args_changes() {
    let (expected, actual, changes) = args(&(1, Any).into_args(), &[Value::BigInt(Some(1)), Value::from("x")], &[], DbBackend::Postgres);
    assert_eq!((expected.as_str(), actual.as_str()), ("[1, _]", "[1, 'x']"));
    assert!(changes.is_empty());

    let (expected, actual, changes) = args(&(7, "a").into_args(), &[Value::Int(Some(8)), Value::from("a")], &[0], DbBackend::Postgres);
    assert_eq!(
      changes,
      [Change {
        expected: 1..2,
        replacement: "8".into()
      }]
    );
    assert_eq!(apply(&expected, &changes), actual);

    let cases: [(Vec<Arg>, Vec<Value>); 4] = [
      ((1,).into_args(), vec![Value::Int(Some(1)), Value::Int(Some(2))]),
      ((2,).into_args(), vec![Value::Int(Some(1)), Value::Int(Some(2))]),
      ((1, 2).into_args(), vec![Value::Int(Some(2))]),
      (().into_args(), vec![Value::Int(Some(1))]),
    ];

    for (expected, values) in cases {
      let (expected, actual, changes) = args(&expected, &values, &[], DbBackend::Postgres);
      assert_eq!(apply(&expected, &changes), actual);
    }

    assert_eq!(literal(&Value::String(None), DbBackend::Postgres), "NULL");
    assert_eq!(literal(&Value::from("it's"), DbBackend::Postgres), r"E'it\'s'");
    assert_eq!(literal(&Value::from("it's"), DbBackend::Sqlite), "'it''s'");
  }

  const BRACES: (&str, &str) = ("{", "}");

  fn values(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
  }

  #[test]
  fn interpolate_placeholders() {
    let interpolated = interpolate(r#"SELECT "id" FROM "cake" WHERE "name" = $1 LIMIT $2"#, &values(&["'Lemon'", "1"]), DbBackend::Postgres, BRACES).unwrap();
    assert_eq!(interpolated.text, r#"SELECT "id" FROM "cake" WHERE "name" = {'Lemon'} LIMIT {1}"#);
    assert_eq!(interpolated.values, [(0, 40..47), (1, 56..57)]);
    assert_eq!(&interpolated.text[40..47], "'Lemon'");

    // Out of order, and reused.
    let interpolated = interpolate("SELECT $2, $1, $2", &values(&["1", "2"]), DbBackend::Postgres, BRACES).unwrap();
    assert_eq!(interpolated.text, "SELECT {2}, {1}, {2}");
    assert_eq!(interpolated.values.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [1, 0, 1]);

    for backend in [DbBackend::MySql, DbBackend::Sqlite] {
      let interpolated = interpolate("SELECT `id` FROM `cake` WHERE `id` = ? AND `name` = ?", &values(&["1", "'x'"]), backend, BRACES).unwrap();
      assert_eq!(interpolated.text, "SELECT `id` FROM `cake` WHERE `id` = {1} AND `name` = {'x'}");
    }

    // Placeholders in strings and comments are not placeholders.
    let interpolated = interpolate("SELECT '$1' -- $1\nFROM t WHERE a = $1", &values(&["'it''s'"]), DbBackend::Postgres, BRACES).unwrap();
    assert_eq!(interpolated.text, "SELECT '$1' -- $1\nFROM t WHERE a = {'it''s'}");
  }

  #[test]
  fn interpolate_mismatched_values() {
    assert_eq!(interpolate("SELECT $1, $2", &values(&["1"]), DbBackend::Postgres, BRACES), None);
    assert_eq!(interpolate("SELECT $1", &values(&["1", "2"]), DbBackend::Postgres, BRACES), None);
    assert_eq!(interpolate("SELECT ?", &values(&[]), DbBackend::MySql, BRACES), None);
  }
}
