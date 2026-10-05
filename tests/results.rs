//! Results: returned rows and models, write results, closures, extra
//! columns, and how mock rows decode.

mod common;

use std::collections::BTreeMap;

use common::cake::{self, cake};
use leadline::{Any, Arg, MockDb, StatementExt};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, DbBackend, DbErr, EntityTrait, ProxyExecResult, QueryFilter, QuerySelect, Value, sea_query::Expr};

#[tokio::test]
async fn select_returning_with_closure() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_select::<cake::Entity>().times(2).returning_with(|stmt| {
    let Some(id) = stmt.arg::<i32>(0) else {
      return Err(DbErr::Custom("no id".into()));
    };

    Ok(vec![cake(id, &format!("Cake #{id}"))])
  });

  let db = mock.connection().await;

  for id in [3, 4] {
    let found = cake::Entity::find_by_id(id).one(&db).await.unwrap();
    assert_eq!(found, Some(cake(id, &format!("Cake #{id}"))));
  }
}

#[tokio::test]
async fn untyped_query_returns_raw_rows() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock
    .expect_query()
    .sql_regex(r"^SELECT COUNT\(\*\)")
    .returning_rows([BTreeMap::from([("count", Value::BigInt(Some(12)))])]);

  let db = mock.connection().await;
  let row = sea_orm::ConnectionTrait::query_one_raw(&db, sea_orm::Statement::from_string(DbBackend::Postgres, r#"SELECT COUNT(*) AS count FROM "cake""#))
    .await
    .unwrap()
    .unwrap();

  assert_eq!(row.try_get::<i64>("", "count").unwrap(), 12);
}

#[tokio::test]
async fn insert_with_returning_on_postgres() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_insert::<cake::Entity>().with_args(("Lemon",)).returning([cake(7, "Lemon")]);

  let db = mock.connection().await;
  let model = cake::ActiveModel {
    name: Set("Lemon".into()),
    ..Default::default()
  }
  .insert(&db)
  .await
  .unwrap();

  assert_eq!(model, cake(7, "Lemon"));
}

#[tokio::test]
async fn insert_last_insert_id_on_postgres_and_mysql() {
  for backend in [DbBackend::Postgres, DbBackend::MySql] {
    let mock = MockDb::new(backend);
    mock.expect_insert::<cake::Entity>().last_insert_id(42);

    let db = mock.connection().await;
    let res = cake::Entity::insert(cake::ActiveModel {
      name: Set("Apple".into()),
      ..Default::default()
    })
    .exec(&db)
    .await
    .unwrap();

    assert_eq!(res.last_insert_id, 42, "{backend:?}");
  }
}

#[tokio::test]
async fn active_model_insert_on_mysql_reselects() {
  let mock = MockDb::new(DbBackend::MySql);

  mock.expect_insert::<cake::Entity>().last_insert_id(5);
  mock.expect_select::<cake::Entity>().with_args((5, 1u64)).returning([cake(5, "Banana")]);

  let db = mock.connection().await;
  let model = cake::ActiveModel {
    name: Set("Banana".into()),
    ..Default::default()
  }
  .insert(&db)
  .await
  .unwrap();

  assert_eq!(model, cake(5, "Banana"));
}

#[tokio::test]
async fn update_reports_rows_affected() {
  let mock = MockDb::new(DbBackend::MySql);

  mock
    .expect_update::<cake::Entity>()
    .with_args(("Stale", Arg::matching(|v| matches!(v, Value::Int(Some(n)) if *n > 10))))
    .rows_affected(3);

  let db = mock.connection().await;
  let res = cake::Entity::update_many()
    .col_expr(cake::Column::Name, Expr::value("Stale"))
    .filter(cake::Column::Id.gt(100))
    .exec(&db)
    .await
    .unwrap();

  assert_eq!(res.rows_affected, 3);
}

#[tokio::test]
async fn delete_returns_error() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_delete::<cake::Entity>().with_args((Any,)).returning_error(DbErr::Custom("boom".into()));

  let db = mock.connection().await;
  let err = cake::Entity::delete_by_id(1).exec(&db).await.unwrap_err();

  assert_eq!(err.to_string(), DbErr::Custom("boom".into()).to_string());
}

#[tokio::test]
async fn exec_with_closure() {
  let mock = MockDb::new(DbBackend::MySql);

  mock.expect_delete::<cake::Entity>().exec_with(|stmt| {
    let ids = stmt.args().len();
    Ok(ProxyExecResult::new(0, ids as u64))
  });

  let db = mock.connection().await;
  let res = cake::Entity::delete_many().filter(cake::Column::Id.is_in([1, 2, 3])).exec(&db).await.unwrap();

  assert_eq!(res.rows_affected, 3);
}

#[tokio::test]
async fn insert_returning_with_closure() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_insert::<cake::Entity>().returning_with(|stmt| {
    let Some(name) = stmt.arg::<String>(0) else {
      return Err(DbErr::Custom("no name".into()));
    };

    Ok(vec![cake(9, &name)])
  });

  let db = mock.connection().await;
  let model = cake::ActiveModel {
    name: Set("Carrot".into()),
    ..Default::default()
  }
  .insert(&db)
  .await
  .unwrap();

  assert_eq!(model, cake(9, "Carrot"));
}

#[tokio::test]
async fn write_response_combines_rows_and_id() {
  let mock = MockDb::new(DbBackend::MySql);

  mock.expect_insert::<cake::Entity>().last_insert_id(42).rows_affected(3);

  let db = mock.connection().await;
  let inserted = cake::Entity::insert_many(["A", "B", "C"].map(|name| cake::ActiveModel {
    name: Set(name.into()),
    ..Default::default()
  }))
  .exec_without_returning(&db)
  .await
  .unwrap();

  assert_eq!(inserted, 3);
}

#[tokio::test]
async fn returning_with_extra_columns() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_select::<cake::Entity>().returning_with_columns([(cake(1, "Chocolate"), [("rank", 3i64)])]);

  let db = mock.connection().await;
  let row = cake::Entity::find().into_json().one(&db).await.unwrap().unwrap();

  assert_eq!(row["name"], "Chocolate");
  assert_eq!(row["rank"], 3);
}

#[tokio::test]
#[should_panic(expected = "extra column `name` is already a column of the model")]
async fn extra_columns_cannot_override_the_model() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_select::<cake::Entity>().returning_with_columns([(cake(1, "Chocolate"), [("name", "Lemon")])]);
}

// Mock rows store their columns in a map sorted by column name: by-name
// decoding is unaffected, but by-position decoding (`into_tuple`) sees the
// columns in alphabetical order, not in SELECT order.

#[tokio::test]
async fn extra_columns_order_does_not_matter() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock
    .expect_select::<cake::Entity>()
    .returning_with_columns([(cake(1, "Chocolate"), [("rank", 3i64), ("bakery", 7i64)])]);
  mock
    .expect_select::<cake::Entity>()
    .returning_with_columns([(cake(1, "Chocolate"), [("bakery", 7i64), ("rank", 3i64)])]);

  let db = mock.connection().await;
  let first = cake::Entity::find().into_json().one(&db).await.unwrap();
  let second = cake::Entity::find().into_json().one(&db).await.unwrap();

  assert_eq!(first, second);
}

#[tokio::test]
async fn into_tuple_decodes_mock_rows_in_alphabetical_order() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_select::<cake::Entity>().times(2).returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;

  // Tuple in alphabetical column order (`id` < `name`): decodes.
  let ok = cake::Entity::find()
    .select_only()
    .column(cake::Column::Id)
    .column(cake::Column::Name)
    .into_tuple::<(i32, String)>()
    .all(&db)
    .await
    .unwrap();
  assert_eq!(ok, vec![(1, "Chocolate".to_string())]);

  // Tuple in SELECT order (`name`, `id`): position 0 is still `id`, so the
  // types do not line up. Against a real database, this would decode.
  let err = cake::Entity::find()
    .select_only()
    .column(cake::Column::Name)
    .column(cake::Column::Id)
    .into_tuple::<(String, i32)>()
    .all(&db)
    .await
    .unwrap_err();
  assert!(matches!(err, DbErr::Type(_)), "{err:?}");
}

#[tokio::test]
async fn into_tuple_aliases_fix_the_order() {
  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_select::<cake::Entity>().returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;

  // Aliases sorting in SELECT order make positions line up; the mock renames
  // the model's columns to the aliases found in the SQL.
  let found = cake::Entity::find()
    .select_only()
    .column_as(cake::Column::Name, "c0_name")
    .column_as(cake::Column::Id, "c1_id")
    .into_tuple::<(String, i32)>()
    .all(&db)
    .await
    .unwrap();

  assert_eq!(found, vec![("Chocolate".to_string(), 1)]);
}

mod ticket {
  use sea_orm::entity::prelude::*;

  #[derive(Clone, Copy, Debug, PartialEq, EnumIter, DeriveActiveEnum)]
  #[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "ticket_status")]
  pub enum Status {
    #[sea_orm(string_value = "open")]
    Open,
    #[sea_orm(string_value = "closed")]
    Closed,
  }

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "ticket")]
  pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub status: Status,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Debug, PartialEq, sea_orm::FromQueryResult)]
struct LabeledTicket {
  id: i32,
  label: ticket::Status,
}

#[tokio::test]
async fn postgres_enum_columns_decode() {
  let mock = MockDb::new(DbBackend::Postgres);
  let tickets = vec![
    ticket::Model { id: 1, status: ticket::Status::Open },
    ticket::Model {
      id: 2,
      status: ticket::Status::Closed,
    },
  ];

  // Enum columns are selected as `CAST("ticket"."status" AS "text")`: the cast
  // type must not be mistaken for an alias…
  mock.expect_select::<ticket::Entity>().sql_contains(r#"CAST("ticket"."status" AS "text")"#).returning(tickets.clone());
  // …while an aliased cast, `CAST(…) AS "label"`, renames the column.
  mock.expect_select::<ticket::Entity>().sql_contains(r#"AS "label""#).returning(tickets.clone());

  let db = mock.connection().await;
  assert_eq!(ticket::Entity::find().all(&db).await.unwrap(), tickets);

  let labeled = ticket::Entity::find()
    .select_only()
    .column(ticket::Column::Id)
    .column_as(ticket::Column::Status, "label")
    .into_model::<LabeledTicket>()
    .all(&db)
    .await
    .unwrap();

  assert_eq!(
    labeled,
    vec![LabeledTicket { id: 1, label: ticket::Status::Open }, LabeledTicket { id: 2, label: ticket::Status::Closed }]
  );
}

#[tokio::test]
async fn aliased_tables_and_columns_decode() {
  use sea_orm::{ConnectionTrait, sea_query::Query};

  let mock = MockDb::new(DbBackend::Postgres);

  // Columns of `cake`, read through the table alias `c`, under column aliases.
  mock.expect_select::<cake::Entity>().returning([cake(1, "Chocolate")]);

  let query = Query::select()
    .expr_as(sea_orm::sea_query::Expr::col(("c", "id")), "cake_id")
    .expr_as(sea_orm::sea_query::Expr::col(("c", "name")), "cake_name")
    .from_as(cake::Entity, "c")
    .to_owned();

  let db = mock.connection().await;
  let row = db.query_one(&query).await.unwrap().unwrap();

  assert_eq!(row.try_get::<i32>("", "cake_id").unwrap(), 1);
  assert_eq!(row.try_get::<String>("", "cake_name").unwrap(), "Chocolate");
}

#[tokio::test]
async fn columns_aliased_several_times_decode() {
  use sea_orm::{ConnectionTrait, sea_query::Expr, sea_query::Query};

  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_select::<cake::Entity>().times(2).returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;

  // The same column under two aliases.
  let twice = Query::select()
    .expr_as(Expr::col(("c", "id")), "first")
    .expr_as(Expr::col(("c", "id")), "second")
    .from_as(cake::Entity, "c")
    .to_owned();
  let row = db.query_one(&twice).await.unwrap().unwrap();

  assert_eq!(row.try_get::<i32>("", "first").unwrap(), 1);
  assert_eq!(row.try_get::<i32>("", "second").unwrap(), 1);

  // The same column under its own name and under an alias.
  let both = Query::select().column(("c", "id")).expr_as(Expr::col(("c", "id")), "other").from_as(cake::Entity, "c").to_owned();
  let row = db.query_one(&both).await.unwrap().unwrap();

  assert_eq!(row.try_get::<i32>("", "id").unwrap(), 1);
  assert_eq!(row.try_get::<i32>("", "other").unwrap(), 1);
}

#[tokio::test]
async fn aliases_named_like_model_columns_and_through_ctes() {
  use sea_orm::{ConnectionTrait, Statement};

  let mock = MockDb::new(DbBackend::Postgres);
  mock.expect_select::<cake::Entity>().times(2).returning([cake(1, "Chocolate")]);

  let db = mock.connection().await;

  // `name` is also a column of the model: it takes the value of `id`.
  let renamed = Statement::from_string(DbBackend::Postgres, r#"SELECT "c"."id" AS "name" FROM "cake" AS "c""#);
  let row = db.query_one_raw(renamed).await.unwrap().unwrap();
  assert_eq!(row.try_get::<i32>("", "name").unwrap(), 1);

  // A CTE stands for the table it reads.
  let cte = Statement::from_string(DbBackend::Postgres, r#"WITH "c" AS (SELECT "id" FROM "cake") SELECT "c"."id" AS "value" FROM "c""#);
  let row = db.query_one_raw(cte).await.unwrap().unwrap();
  assert_eq!(row.try_get::<i32>("", "value").unwrap(), 1);
}

#[tokio::test]
async fn mysql_inserts_returning_models_report_their_key() {
  let mock = MockDb::new(DbBackend::MySql);

  // MySQL has no RETURNING: the insert reports the model's key as the last
  // inserted ID, then SeaORM selects the row back.
  mock.expect_insert::<cake::Entity>().returning([cake(5, "Banana")]);
  mock.expect_select::<cake::Entity>().with_args((5, 1u64)).returning([cake(5, "Banana")]);

  let db = mock.connection().await;
  let model = cake::ActiveModel {
    name: Set("Banana".into()),
    ..Default::default()
  }
  .insert(&db)
  .await
  .unwrap();

  assert_eq!(model, cake(5, "Banana"));
}

#[tokio::test]
async fn mysql_inserts_returning_computed_models_report_their_key() {
  let mock = MockDb::new(DbBackend::MySql);

  // The closure's models give the last inserted ID too, as `returning` does.
  mock.expect_insert::<cake::Entity>().returning_with(|stmt| {
    let name = stmt.arg::<String>(0).unwrap_or_default();
    Ok(vec![cake(5, &name)])
  });
  mock.expect_select::<cake::Entity>().with_args((5, 1u64)).returning([cake(5, "Banana")]);

  let db = mock.connection().await;
  let model = cake::ActiveModel {
    name: Set("Banana".into()),
    ..Default::default()
  }
  .insert(&db)
  .await
  .unwrap();

  assert_eq!(model, cake(5, "Banana"));
}
