//! Failures and their reports: unexpected statements, missing results, the
//! check run when the mock is dropped, and `check` / `verify`.

mod common;

use common::{
  cake::{self, cake},
  problems,
};
use leadline::MockDb;
use sea_orm::{DbBackend, EntityTrait};

#[tokio::test]
#[should_panic(expected = "leadline: unexpected SELECT")]
async fn unexpected_query_fails_the_test() {
  let mock = MockDb::new(DbBackend::Postgres);
  let db = mock.connection().await;

  let _ = cake::Entity::find().all(&db).await;
}

#[tokio::test]
async fn unexpected_query_in_spawned_task_is_reported_on_drop() {
  let result = tokio::spawn(async {
    let mock = MockDb::new(DbBackend::Postgres);
    let db = mock.connection().await;

    // The panic is swallowed by the inner task, as code under test might do.
    let inner = tokio::spawn(async move { cake::Entity::find().all(&db).await });
    assert!(inner.await.is_err());
  })
  .await;

  let panic = result.unwrap_err().into_panic();
  let message = panic.downcast_ref::<String>().unwrap();
  assert!(message.starts_with("leadline: 1 problem:\n  - unexpected SELECT: no expectation was set\n"), "{message}");
}

#[tokio::test]
async fn check_reports_and_disarms_drop() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_delete::<cake::Entity>().rows_affected(1);

  let err = mock.check().unwrap_err();
  assert_eq!(err.problems(), ["expectation not met: DELETE on `cake` with any SQL"]);
}

#[tokio::test]
#[should_panic(expected = "leadline: 1 problem:\n  - expectation not met: DELETE on `cake` with any SQL")]
async fn drop_panics_on_unmet_expectation() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_delete::<cake::Entity>().rows_affected(1);
}

#[tokio::test]
#[should_panic(expected = "leadline: 1 problem:\n  - expectation not met: DELETE on `cake` with any SQL")]
async fn verify_panics_on_unmet_expectation() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_delete::<cake::Entity>().rows_affected(1);

  mock.verify();
}

#[tokio::test]
async fn verify_as_a_checkpoint() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_delete::<cake::Entity>().rows_affected(1);

  let db = mock.connection().await;
  cake::Entity::delete_by_id(1).exec(&db).await.unwrap();

  // Everything scripted so far was consumed; script the next phase.
  mock.verify();
  mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);

  assert!(cake::Entity::find().all(&db).await.unwrap().is_empty());
}

#[tokio::test]
#[should_panic(expected = "leadline: unexpected DELETE: every expectation was already consumed")]
async fn reports_consumed_expectations() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_delete::<cake::Entity>().rows_affected(1);

  let db = mock.connection().await;
  cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  let _ = cake::Entity::delete_by_id(2).exec(&db).await;
}

#[tokio::test]
async fn reports_why_optional_expectations_did_not_match() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_select::<cake::Entity>().maybe().returning::<cake::Model>([]);

  let db = mock.connection().await;
  let problems = problems(&mock, async move {
    let _ = cake::Entity::delete_by_id(1).exec(&db).await;
  })
  .await;

  assert!(
    problems[0].ends_with("none of the remaining optional expectations matches it:\n      - SELECT on `cake` with any SQL (optional): expected a SELECT statement, got DELETE"),
    "{problems:?}"
  );
}

#[tokio::test]
async fn unordered_reports_why_nothing_matched() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.unordered();

  mock.expect_delete::<cake::Entity>().rows_affected(1);
  mock.expect_update::<cake::Entity>().rows_affected(1);

  let db = mock.connection().await;
  let problems = problems(&mock, async move {
    let _ = cake::Entity::find().all(&db).await;
  })
  .await;

  assert!(
    problems[0].ends_with(
      "none of the pending expectations matches it:\n      - DELETE on `cake` with any SQL: expected a DELETE statement, got SELECT\n      - UPDATE on `cake` with any SQL: expected an UPDATE statement, got SELECT"
    ),
    "{problems:?}"
  );
}

#[tokio::test]
#[should_panic(expected = "leadline: unexpected SELECT: it matches an expectation that has no result")]
async fn unfinished_expectation_fails_when_hit() {
  let mock = MockDb::new(DbBackend::Postgres);

  // Discarding the pending builder explicitly silences `#[must_use]`.
  let _ = mock.expect_select::<cake::Entity>().sql_contains("cake");

  let db = mock.connection().await;
  let _ = cake::Entity::find().all(&db).await;
}

#[tokio::test]
async fn unfinished_expectation_is_reported() {
  let mock = MockDb::new(DbBackend::Postgres);
  let _ = mock.expect_delete::<cake::Entity>().with_args((1,));

  let err = mock.check().unwrap_err();
  assert_eq!(err.problems(), ["expectation has no result: DELETE on `cake` with any SQL and args [Int(Some(1))]"]);
}

/// Runs `test` on its own thread and runtime, failing if it does not finish
/// in time: a deadlock would otherwise hang the whole suite.
fn without_deadlock(test: impl Future<Output = ()> + Send + 'static) {
  use std::{sync::mpsc, thread, time::Duration};

  let (done, finished) = mpsc::channel();

  thread::spawn(move || {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(test);
    let _ = done.send(());
  });

  match finished.recv_timeout(Duration::from_secs(10)) {
    Ok(()) => {}
    Err(mpsc::RecvTimeoutError::Timeout) => panic!("the mock deadlocked"),
    Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the test panicked"),
  }
}

#[test]
fn callbacks_can_call_the_mock() {
  use leadline::Arg;
  use sea_orm::{ConnectionTrait, ProxyExecResult, Statement};

  without_deadlock(async {
    let mock = MockDb::new(DbBackend::Postgres);

    // Every kind of callback inspects the mock while a statement is handled.
    let inspect = mock.clone();
    mock.expect_select::<cake::Entity>().sql_fn(move |_| !inspect.statements().is_empty()).returning([cake(1, "Chocolate")]);

    let inspect = mock.clone();
    mock.expect_select::<cake::Entity>().returning_with(move |_| Ok(vec![cake(inspect.statements().len() as i32, "Lemon")]));

    let inspect = mock.clone();
    mock.expect_delete::<cake::Entity>().with_args((Arg::matching(move |_| inspect.check().is_err()),)).rows_affected(1);

    let inspect = mock.clone();
    mock.expect_statement().exec_with(move |_| Ok(ProxyExecResult::new(0, inspect.statements().len() as u64)));

    let db = mock.connection().await;

    assert_eq!(cake::Entity::find().all(&db).await.unwrap(), vec![cake(1, "Chocolate")]);
    assert_eq!(cake::Entity::find().all(&db).await.unwrap(), vec![cake(2, "Lemon")]);
    cake::Entity::delete_by_id(1).exec(&db).await.unwrap();

    let res = db.execute_raw(Statement::from_string(DbBackend::Postgres, "VACUUM")).await.unwrap();
    assert_eq!(res.rows_affected(), 4);
  });
}

#[tokio::test]
async fn writes_read_back_without_rows_fail_clearly() {
  use sea_orm::{ActiveModelTrait, ActiveValue::Set, ActiveValue::Unchanged};

  let mock = MockDb::new(DbBackend::Postgres);

  // On Postgres, `ActiveModel::update` reads the updated row back: an
  // affected row count is not enough.
  mock.expect_update::<cake::Entity>().rows_affected(1);

  let db = mock.connection().await;
  let problems = problems(&mock, async move {
    let _ = cake::ActiveModel {
      id: Unchanged(1),
      name: Set("Lemon".into()),
    }
    .update(&db)
    .await;
  })
  .await;

  assert!(
    problems[0]
      .ends_with("it reads the written rows back (`RETURNING` on this backend), but its expectation has no rows to return: complete it with `.returning(..)` (`.returning::<Model>([])` for none)"),
    "{problems:?}"
  );
}

#[tokio::test]
async fn untyped_writes_read_back_without_rows_fail_clearly() {
  use sea_orm::{ConnectionTrait, Statement};

  let mock = MockDb::new(DbBackend::Postgres);

  // An untyped expectation, but the statement is a write reading rows back.
  mock.expect_statement().rows_affected(1);

  let db = mock.connection().await;
  let stmt = Statement::from_string(DbBackend::Postgres, r#"INSERT INTO "cake" ("name") VALUES ('Lemon') RETURNING "id""#);
  let problems = problems(&mock, async move {
    let _ = db.query_all_raw(stmt).await;
  })
  .await;

  assert!(
    problems[0].ends_with("but its expectation has no rows to return: complete it with `.returning_rows(..)` (an empty list for none)"),
    "{problems:?}"
  );
}
