//! Order and number of calls: ordered and unordered mocks, `maybe` and
//! `times`.

mod common;

use common::{
  cake::{self, cake},
  problems,
};
use leadline::MockDb;
use sea_orm::{DbBackend, EntityTrait};

#[tokio::test]
async fn ordered_by_default() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_delete::<cake::Entity>().rows_affected(1);
  mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);

  let db = mock.connection().await;
  let problems = problems(&mock, async move {
    let _ = cake::Entity::find().all(&db).await;
  })
  .await;

  assert!(
    problems[0].contains("the next expectation does not match it (DELETE on `cake` with any SQL): expected a DELETE statement, got SELECT"),
    "{problems:?}"
  );
}

#[tokio::test]
async fn unordered_mode() {
  let mock = MockDb::new(DbBackend::Postgres).unordered();

  mock.expect_delete::<cake::Entity>().rows_affected(1);
  mock.expect_select::<cake::Entity>().returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;
  assert_eq!(cake::Entity::find().all(&db).await.unwrap().len(), 1);
  assert_eq!(cake::Entity::delete_by_id(1).exec(&db).await.unwrap().rows_affected, 1);
}

#[tokio::test]
async fn maybe_can_be_skipped_or_used() {
  for call_optional in [false, true] {
    let mock = MockDb::new(DbBackend::Postgres);

    mock.expect_select::<cake::Entity>().maybe().returning([cake(1, "Cached")]);
    mock.expect_delete::<cake::Entity>().rows_affected(1);

    let db = mock.connection().await;

    if call_optional {
      cake::Entity::find().all(&db).await.unwrap();
    }
    cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  }
}

#[tokio::test]
#[should_panic(expected = "unexpected SELECT")]
async fn skipped_maybe_cannot_be_called_later_in_order() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_select::<cake::Entity>().maybe().returning::<cake::Model>([]);
  mock.expect_delete::<cake::Entity>().rows_affected(1);

  let db = mock.connection().await;
  cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  let _ = cake::Entity::find().all(&db).await;
}

#[tokio::test]
async fn times_then_next() {
  let mock = MockDb::new(DbBackend::MySql);

  mock.expect_delete::<cake::Entity>().times(2).rows_affected(1);
  mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);

  let db = mock.connection().await;
  cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  cake::Entity::delete_by_id(2).exec(&db).await.unwrap();
  cake::Entity::find().all(&db).await.unwrap();
}

#[tokio::test]
async fn times_not_reached() {
  let mock = MockDb::new(DbBackend::MySql);
  mock.expect_delete::<cake::Entity>().times(2).rows_affected(1);

  let db = mock.connection().await;
  cake::Entity::delete_by_id(1).exec(&db).await.unwrap();

  assert_eq!(mock.check().unwrap_err().problems(), ["expectation not met: DELETE on `cake` with any SQL (1/2 calls)"]);
}
