//! Transactions: explicit and closure-based, commits, rollbacks, and
//! code running several statements in one.

mod common;

use common::schema::*;
use leadline::{Any, MockDb};
use sea_orm::{ColumnTrait, DbBackend, DbErr, EntityTrait, QueryFilter, TransactionTrait};

#[tokio::test]
async fn transaction_commit() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_begin();
  mock.expect_delete::<cake::Entity>().rows_affected(1);
  mock.expect_commit();

  let db = mock.connection().await;
  db.transaction::<_, _, DbErr>(|tx| {
    Box::pin(async move {
      cake::Entity::delete_by_id(1).exec(tx).await?;
      Ok(())
    })
  })
  .await
  .unwrap();
}

#[tokio::test]
async fn transaction_rollback_on_error() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_begin();
  mock.expect_delete::<cake::Entity>().returning_error(DbErr::Custom("boom".into()));
  mock.expect_rollback();

  let db = mock.connection().await;
  let res = db
    .transaction::<_, (), DbErr>(|tx| {
      Box::pin(async move {
        cake::Entity::delete_by_id(1).exec(tx).await?;
        Ok(())
      })
    })
    .await;

  assert!(res.is_err());
}

#[tokio::test]
async fn transactions_ignored_unless_expected() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_delete::<cake::Entity>().rows_affected(1);

  let db = mock.connection().await;
  let tx = db.begin().await.unwrap();
  cake::Entity::delete_by_id(1).exec(&tx).await.unwrap();
  tx.commit().await.unwrap();
}

/// Deletes a bakery and everything that depends on it.
async fn close_bakery(db: &impl TransactionTrait, id: i32) -> Result<(), DbErr> {
  db.transaction::<_, _, DbErr>(|tx| {
    Box::pin(async move {
      let cakes = cake::Entity::find().filter(cake::Column::BakeryId.eq(id)).all(tx).await?;
      let cake_ids: Vec<i32> = cakes.iter().map(|cake| cake.id).collect();

      cake_filling::Entity::delete_many().filter(cake_filling::Column::CakeId.is_in(cake_ids.clone())).exec(tx).await?;
      cake::Entity::delete_many().filter(cake::Column::Id.is_in(cake_ids)).exec(tx).await?;
      bakery::Entity::delete_by_id(id).exec(tx).await?;

      Ok(())
    })
  })
  .await
  .map_err(|err| match err {
    sea_orm::TransactionError::Connection(err) | sea_orm::TransactionError::Transaction(err) => err,
  })
}

#[tokio::test]
async fn cascading_delete_in_a_transaction() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_begin();
  mock
    .expect_select::<cake::Entity>()
    .matching(cake::Entity::find().filter(cake::Column::BakeryId.eq(1)))
    .returning([cake(1, "Chocolate", Some(1)), cake(3, "Carrot", Some(1))]);
  mock.expect_delete::<cake_filling::Entity>().with_args((1, 3)).rows_affected(4);
  mock.expect_delete::<cake::Entity>().with_args((1, 3)).rows_affected(2);
  mock.expect_delete::<bakery::Entity>().matching(bakery::Entity::delete_by_id(1)).rows_affected(1);
  mock.expect_commit();

  let db = mock.connection().await;
  close_bakery(&db, 1).await.unwrap();
}

#[tokio::test]
async fn cascading_delete_rolls_back_on_failure() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_begin();
  mock.expect_select::<cake::Entity>().returning([cake(1, "Chocolate", Some(1))]);
  mock.expect_delete::<cake_filling::Entity>().with_args((Any,)).rows_affected(1);
  mock.expect_delete::<cake::Entity>().returning_error(DbErr::Custom("foreign key violation".into()));
  mock.expect_rollback();

  let db = mock.connection().await;
  let err = close_bakery(&db, 1).await.unwrap_err();

  assert_eq!(err.to_string(), DbErr::Custom("foreign key violation".into()).to_string());
}

#[tokio::test]
#[should_panic(expected = "unexpected BEGIN")]
async fn statement_matchers_apply_to_transactions() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.strict_transactions();

  // An untyped expectation accepts any kind of statement, but its SQL
  // matcher must still match: a BEGIN is not a COMMIT.
  mock.expect_statement().sql("COMMIT").rows_affected(0);

  let db = mock.connection().await;
  let _ = db.begin().await;
}

#[tokio::test]
async fn statement_expectations_can_match_transactions() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_begin();
  mock.expect_statement().sql("COMMIT").rows_affected(0);

  let db = mock.connection().await;
  db.begin().await.unwrap().commit().await.unwrap();
}

#[tokio::test]
async fn transaction_statements_use_the_mock_backend() {
  let mock = MockDb::new(DbBackend::MySql);

  mock.expect_begin();
  mock.expect_statement().sql_fn(|stmt| stmt.db_backend == DbBackend::MySql).sql("COMMIT").rows_affected(0);

  let db = mock.connection().await;
  db.begin().await.unwrap().commit().await.unwrap();

  assert!(mock.statements().iter().all(|stmt| stmt.db_backend == DbBackend::MySql));
}

#[tokio::test]
async fn statement_expectations_answer_unchecked_transactions() {
  let mock = MockDb::new(DbBackend::Postgres);

  // No transaction is expected, so transactions are not checked: the BEGIN
  // is ignored, but the COMMIT is answered by the expectation matching it.
  mock.expect_statement().sql("COMMIT").rows_affected(0);

  let db = mock.connection().await;
  db.begin().await.unwrap().commit().await.unwrap();

  mock.verify();
}
