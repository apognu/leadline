//! Failures as the user sees them: each test runs a failing scenario, and
//! checks the headline and the diagnostic it panics with. The full rendering
//! is checked by the snapshot tests of `src/render.rs`.
//!
//! To look at the diagnostics, in color when stderr and stdout are terminals:
//!
//! ```text
//! cargo test --test diagnostics -- --nocapture --test-threads 1
//! ```

mod common;

use common::{
  cake::{self, cake},
  schema::bakery,
};
use leadline::MockDb;
use sea_orm::{ColumnTrait, DbBackend, EntityTrait, QueryFilter, QueryOrder};

/// Run `test`, which must panic, and return its panic message, made plain by
/// `plain`. The panic comes from a failing statement, or from the mock being
/// dropped at the end of `test` with problems left. `test` is spawned as a
/// task, so that its panic fails the task rather than this test.
async fn failure(test: impl Future<Output = ()> + Send + 'static) -> String {
  let panic = tokio::spawn(test).await.expect_err("the test did not fail").into_panic();
  let message = panic.downcast::<String>().expect("the panic has a message");

  plain(&message)
}

/// Make a diagnostic read the same whether it was rendered with colors or not,
/// so that tests pass in both cases: remove the color codes, remove the braces
/// that surround bound values without colors, and say that values are shown
/// "in color", as the legend does with colors.
///
/// Every brace is removed, not only those around values: the statements of
/// these tests contain none of their own.
///
/// `= {1} LIMIT {1}` (without colors) and `= 1 LIMIT 1` (with colors, codes
/// removed) both give `= 1 LIMIT 1`.
fn plain(message: &str) -> String {
  let mut text = String::with_capacity(message.len());
  let mut chars = message.chars();

  while let Some(c) = chars.next() {
    match c {
      // An escape code runs until its final letter, such as `m` for styles.
      '\x1b' => {
        for c in chars.by_ref() {
          if c.is_ascii_alphabetic() {
            break;
          }
        }
      }
      '{' | '}' => {}
      c => text.push(c),
    }
  }

  text.replace("between braces", "in color")
}

/// Check that `message` starts with `headline`, and contains `fragments`.
#[track_caller]
fn assert_diagnostic(message: &str, headline: &str, fragments: &[&str]) {
  assert_eq!(message.lines().next(), Some(headline), "{message}");

  for fragment in fragments {
    assert!(message.contains(fragment), "missing `{fragment}` in:\n{message}");
  }
}

/// A statement differing from the expected one by its `LIMIT`, which `.one()`
/// adds.
#[tokio::test]
async fn statement_differs() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(1)).returning([cake(1, "Chocolate")]);

    let db = mock.connection().await;
    let _ = cake::Entity::find_by_id(1).one(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected SELECT: the next expectation does not match it",
    &[
      "error: unexpected SELECT",
      "--> tests/diagnostics.rs:",
      "the next expectation",
      r#"| SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."id" = 1 LIMIT 1"#,
      "= help: they only differ by LIMIT/OFFSET, which `.one()` and paginators add: use `matching_ignoring_limit`",
      "= note: bound values are shown in place of their placeholders, in color",
    ],
  );
}

/// A statement whose filter and order differ from the expected ones.
#[tokio::test]
async fn query_differs() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres);

    mock
      .expect_select::<cake::Entity>()
      .matching(cake::Entity::find().filter(cake::Column::Name.eq("Chocolate")))
      .returning([cake(1, "Chocolate")]);

    let db = mock.connection().await;
    let _ = cake::Entity::find().filter(cake::Column::Name.contains("choc")).order_by_asc(cake::Column::Id).all(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected SELECT: the next expectation does not match it",
    &[
      r#"- SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."name" = 'Chocolate'"#,
      r#"+ SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."name" LIKE '%choc%' ORDER BY "cake"."id" ASC"#,
    ],
  );
}

/// A bound value differing from the expected argument.
#[tokio::test]
async fn arguments_differ() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_delete::<cake::Entity>().with_args((7,)).rows_affected(1);

    let db = mock.connection().await;
    let _ = cake::Entity::delete_by_id(8).exec(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected DELETE: the next expectation does not match it",
    &[r#"| DELETE FROM "cake" WHERE "cake"."id" = 8"#, "^ expected 7", "::: tests/diagnostics.rs:", "the next expectation"],
  );
}

/// A bound value that reads the same as the expected one, but is of another
/// type: an `i64` where the expected statement has an `i32`.
#[tokio::test]
async fn values_differ_by_type() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres);
    mock
      .expect_select::<cake::Entity>()
      .matching(cake::Entity::find().filter(cake::Column::Id.eq(1)))
      .returning([cake(1, "Chocolate")]);

    let db = mock.connection().await;
    let _ = cake::Entity::find().filter(cake::Column::Id.eq(1i64)).all(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected SELECT: the next expectation does not match it",
    &[
      r#"| SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."id" = 1"#,
      "::: tests/diagnostics.rs:",
      "the next expectation",
      "= note: the values differ by type: expected [Int(Some(1))], received [BigInt(Some(1))]",
      "= note: bound values are shown in place of their placeholders, in color",
    ],
  );
}

/// A `SELECT` where a `DELETE` is expected.
#[tokio::test]
async fn kind_differs() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_delete::<cake::Entity>().rows_affected(1);

    let db = mock.connection().await;
    let _ = cake::Entity::find().all(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected SELECT: the next expectation does not match it",
    &["^^^^^^ this is a SELECT statement", "= note: expected a DELETE statement"],
  );
}

/// A statement matching none of the expectations of an unordered mock.
#[tokio::test]
async fn no_candidate_matches() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres).unordered();
    mock.expect_delete::<cake::Entity>().rows_affected(1);
    mock.expect_select::<cake::Entity>().sql_contains("ORDER BY").returning::<cake::Model>([]);

    let db = mock.connection().await;
    let _ = cake::Entity::find().all(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected SELECT: none of the pending expectations matches it",
    &[
      "= note: none of the 2 pending expectations matches it",
      "= note: 1 expectation of another kind is not shown",
      "note: candidate 1 of 1",
      "= note: expected SQL containing `ORDER BY`",
    ],
  );
}

/// A `SELECT` on an unordered mock, with several kinds of pending
/// expectations: the `SELECT`s on the same table are shown in full, the one on
/// another table in one line, and the `DELETE` and `INSERT` are only counted.
#[tokio::test]
async fn unordered_closest_candidates() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres).unordered();
    mock.expect_delete::<cake::Entity>().rows_affected(1);
    mock.expect_insert::<cake::Entity>().last_insert_id(1);
    mock.expect_select::<bakery::Entity>().returning::<bakery::Model>([]);
    mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(2)).returning([cake(2, "Lemon")]);

    mock
      .expect_select::<cake::Entity>()
      .matching_ignoring_limit(cake::Entity::find().filter(cake::Column::Name.eq("Chocolate")))
      .returning([cake(1, "Chocolate")]);

    let db = mock.connection().await;
    let _ = cake::Entity::find_by_id(1).one(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected SELECT: none of the pending expectations matches it",
    &[
      "= note: none of the 5 pending expectations matches it",
      "= note: on another table: SELECT on `bakery` with any SQL (tests/diagnostics.rs:",
      "= note: 2 expectations of other kinds are not shown",
      "note: candidate 1 of 2",
      r#"- SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."id" = 2"#,
      r#"+ SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."id" = 1 LIMIT 1"#,
      "note: candidate 2 of 2",
      r#"- SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."name" = 'Chocolate'"#,
      r#"+ SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."id" = 1"#,
      "= note: LIMIT and OFFSET are ignored",
    ],
  );
}

/// A `SELECT` on an unordered mock where only writes are pending: they are
/// listed, since nothing closer is.
#[tokio::test]
async fn unordered_only_other_kinds() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres).unordered();
    mock.expect_delete::<cake::Entity>().rows_affected(1);
    mock.expect_update::<cake::Entity>().rows_affected(1);

    let db = mock.connection().await;
    let _ = cake::Entity::find().all(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected SELECT: none of the pending expectations matches it",
    &[
      "= note: of another kind: DELETE on `cake` with any SQL (tests/diagnostics.rs:",
      "= note: of another kind: UPDATE on `cake` with any SQL (tests/diagnostics.rs:",
    ],
  );

  assert!(!message.contains("candidate"), "{message}");
}

/// A statement sent when no expectation is left.
#[tokio::test]
async fn no_expectation_left() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_select::<cake::Entity>().returning([cake(1, "Chocolate")]);

    let db = mock.connection().await;
    let _ = cake::Entity::find().all(&db).await;
    let _ = cake::Entity::find().all(&db).await;
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: unexpected SELECT: every expectation was already consumed",
    &[r#"| SELECT "cake"."id", "cake"."name" FROM "cake""#, "= note: every expectation was already consumed"],
  );
}

/// Expectations left unmet, reported when the mock is dropped.
#[tokio::test]
async fn unmet_on_drop() {
  let message = failure(async {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_select::<cake::Entity>().returning([cake(1, "Chocolate")]);
    mock.expect_delete::<cake::Entity>().with_args((1,)).rows_affected(1);
  })
  .await;

  assert_diagnostic(
    &message,
    "leadline: 2 problems:",
    &[
      "  - expectation not met: SELECT on `cake` with any SQL\n  - expectation not met: DELETE on `cake` with any SQL and args [Int(Some(1))]\n",
      "error: expectation not met: SELECT on `cake` with any SQL",
      "error: expectation not met: DELETE on `cake` with any SQL and args [Int(Some(1))]",
      "--> tests/diagnostics.rs:",
      "expected here",
    ],
  );
}
