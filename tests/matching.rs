//! Matching statements: query builders, SQL and argument matchers, and the
//! table check of typed expectations.

mod common;

use common::cake::{self, cake};
use common::schema::filling;
use leadline::{Any, MockDb};
use sea_orm::{ColumnTrait, DbBackend, EntityTrait, QueryFilter, QuerySelect};

#[tokio::test]
async fn select_one_matching_query_builder() {
  let mock = MockDb::new(DbBackend::Postgres);

  // `.one()` adds `LIMIT 1`, so the expected query must too.
  mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(1).limit(1)).returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;
  let found = cake::Entity::find_by_id(1).one(&db).await.unwrap();

  assert_eq!(found, Some(cake(1, "Chocolate")));
}

#[tokio::test]
async fn select_all_with_sql_and_args() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock
    .expect_select::<cake::Entity>()
    .sql_contains(r#"WHERE "cake"."name" LIKE $1"#)
    .with_args(("%cake%",))
    .returning([cake(1, "Cheesecake"), cake(2, "Pancake")]);

  let db = mock.connection().await;
  let found = cake::Entity::find().filter(cake::Column::Name.contains("cake")).all(&db).await.unwrap();

  assert_eq!(found, vec![cake(1, "Cheesecake"), cake(2, "Pancake")]);
}

#[tokio::test]
#[should_panic(expected = "but it does not match statement")]
async fn mismatched_query_fails_the_test() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock
    .expect_select::<cake::Entity>()
    .matching(cake::Entity::find().filter(cake::Column::Id.eq(1)))
    .returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;
  let _ = cake::Entity::find().filter(cake::Column::Id.eq(2)).all(&db).await;
}

#[tokio::test]
#[should_panic(expected = "use `matching_ignoring_limit`")]
async fn strict_match_hints_at_limit() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(1)).returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;
  let _ = cake::Entity::find_by_id(1).one(&db).await;
}

#[tokio::test]
async fn matching_ignoring_limit() {
  for backend in [DbBackend::Postgres, DbBackend::MySql, DbBackend::Sqlite] {
    let mock = MockDb::new(backend);

    mock
      .expect_select::<cake::Entity>()
      .matching_ignoring_limit(cake::Entity::find_by_id(1))
      .returning([cake(1, "Chocolate")]);
    mock.expect_select::<cake::Entity>().matching_ignoring_limit(cake::Entity::find()).returning([cake(2, "Lemon")]);

    let db = mock.connection().await;
    let one = cake::Entity::find_by_id(1).one(&db).await.unwrap();
    let page = cake::Entity::find().limit(10).offset(20).all(&db).await.unwrap();

    assert_eq!(one, Some(cake(1, "Chocolate")), "{backend:?}");
    assert_eq!(page, vec![cake(2, "Lemon")], "{backend:?}");
  }
}

#[tokio::test]
async fn delete_with_specific_filters() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock
    .expect_delete::<cake::Entity>()
    .matching(cake::Entity::delete_many().filter(cake::Column::Name.like("%stale%")).filter(cake::Column::Id.lt(100)))
    .rows_affected(4);
  mock.expect_delete::<cake::Entity>().matching(cake::Entity::delete_by_id(7)).rows_affected(1);
  mock.expect_delete::<cake::Entity>().sql_contains(r#"WHERE "cake"."id" IN ("#).with_args((1, Any, 3)).rows_affected(3);

  let db = mock.connection().await;

  let res = cake::Entity::delete_many()
    .filter(cake::Column::Name.like("%stale%"))
    .filter(cake::Column::Id.lt(100))
    .exec(&db)
    .await
    .unwrap();
  assert_eq!(res.rows_affected, 4);

  let res = cake::Entity::delete_by_id(7).exec(&db).await.unwrap();
  assert_eq!(res.rows_affected, 1);

  let res = cake::Entity::delete_many().filter(cake::Column::Id.is_in([1, 2, 3])).exec(&db).await.unwrap();
  assert_eq!(res.rows_affected, 3);
}

#[tokio::test]
#[should_panic(expected = r#"but it does not match statement `DELETE FROM "cake" WHERE "cake"."id" = $1` with [Int(Some(7))]"#)]
async fn delete_with_wrong_filter_fails() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_delete::<cake::Entity>().matching(cake::Entity::delete_by_id(7)).rows_affected(1);

  let db = mock.connection().await;
  let _ = cake::Entity::delete_by_id(8).exec(&db).await;
}

#[tokio::test]
async fn sql_matchers_accumulate() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock
    .expect_select::<cake::Entity>()
    .sql_contains(r#"FROM "cake""#)
    .sql_regex(r#"WHERE "cake"."id" = \$1"#)
    .returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;
  cake::Entity::find_by_id(1).all(&db).await.unwrap();
}

#[tokio::test]
#[should_panic(expected = r#"with SQL containing `FROM "cake"` and SQL containing `ORDER BY`, but it does not match SQL containing `ORDER BY`"#)]
async fn every_sql_matcher_must_match() {
  let mock = MockDb::new(DbBackend::Postgres);

  // The first matcher passes: the second one must still be checked.
  mock
    .expect_select::<cake::Entity>()
    .sql_contains(r#"FROM "cake""#)
    .sql_contains("ORDER BY")
    .returning::<cake::Model>([]);

  let db = mock.connection().await;
  let _ = cake::Entity::find().all(&db).await;
}

#[tokio::test]
#[should_panic(expected = r#"it targets "cake", not "filling""#)]
async fn wrong_entity_is_rejected() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_select::<filling::Entity>().returning::<filling::Model>([]);

  let db = mock.connection().await;
  let _ = cake::Entity::find().all(&db).await;
}

#[tokio::test]
async fn data_modifying_ctes_are_classified_by_their_body() {
  use sea_orm::{
    ConnectionTrait,
    sea_query::{CommonTableExpression, Query, WithClause},
  };

  let mock = MockDb::new(DbBackend::Postgres);

  // `WITH … DELETE` starts like a SELECT, but deletes.
  mock.expect_delete::<cake::Entity>().rows_affected(2);

  let stale = Query::select().column(cake::Column::Id).from(cake::Entity).and_where(cake::Column::Name.eq("Stale")).to_owned();
  let delete = Query::delete()
    .from_table(cake::Entity)
    .and_where(cake::Column::Id.in_subquery(Query::select().column("id").from("stale").to_owned()))
    .to_owned()
    .with(WithClause::new().cte(CommonTableExpression::new().query(stale).table_name("stale").to_owned()).to_owned());

  let db = mock.connection().await;
  let res = db.execute(&delete).await.unwrap();

  assert!(mock.statements()[0].sql.starts_with("WITH"), "{}", mock.statements()[0].sql);
  assert_eq!(res.rows_affected(), 2);
}
