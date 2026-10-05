//! SeaORM 2.0's extended models: the smart entity loader, `ModelEx` results
//! and nested `ActiveModel` saves.

mod common;

use common::schema::*;
use leadline::MockDb;
use sea_orm::{
  DatabaseConnection, DbBackend, DbErr, EntityLoaderTrait, EntityTrait, ModelTrait, QueryOrder, TryIntoModel,
  entity::prelude::{HasMany, HasOne},
};

#[tokio::test]
async fn smart_entity_loader() {
  let mock = MockDb::new(DbBackend::Postgres);
  let profile = bakery_profile::Model {
    id: 10,
    bakery_id: 1,
    description: "Open since 1902".into(),
  };

  // 1-1 relations are joined into the main query.
  mock
    .expect_select::<bakery::Entity>()
    .sql_contains(r#"LEFT JOIN "bakery_profile""#)
    .returning_rows([(bakery(1, "Main Street"), Some(profile.clone()))]);

  // 1-N relations are loaded in a second query.
  mock
    .expect_select::<cake::Entity>()
    .sql_contains(r#"WHERE ("cake"."bakery_id") IN ("#)
    .with_args((1,))
    .returning([cake(1, "Chocolate", Some(1)), cake(2, "Lemon", Some(1))]);

  let db = mock.connection().await;
  let found = bakery::Entity::load().filter_by_id(1).with(bakery_profile::Entity).with(cake::Entity).one(&db).await.unwrap().unwrap();

  assert_eq!(found.name, "Main Street");
  assert_eq!(found.profile.as_ref().map(|p| p.description.as_str()), Some("Open since 1902"));
  assert_eq!(found.cakes.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Chocolate", "Lemon"]);
}

/// Code under test: takes a plain `DatabaseConnection`, returns a `ModelEx`
/// graph (bakery → profile, bakery → cakes → fillings).
async fn bakery_details(db: &DatabaseConnection, id: i32) -> Result<Option<bakery::ModelEx>, DbErr> {
  bakery::Entity::load().filter_by_id(id).with(bakery_profile::Entity).with((cake::Entity, filling::Entity)).one(db).await
}

#[tokio::test]
async fn function_returning_model_ex() {
  let mock = MockDb::new(DbBackend::Postgres);
  let profile = bakery_profile::Model {
    id: 10,
    bakery_id: 1,
    description: "Open since 1902".into(),
  };

  // 1-1: joined into the main query.
  mock
    .expect_select::<bakery::Entity>()
    .sql_contains(r#"LEFT JOIN "bakery_profile""#)
    .returning_rows([(bakery(1, "Main Street"), Some(profile.clone()))]);

  // 1-N: cakes of the loaded bakery.
  mock
    .expect_select::<cake::Entity>()
    .sql_contains(r#"WHERE ("cake"."bakery_id") IN ("#)
    .with_args((1,))
    .returning([cake(1, "Chocolate", Some(1)), cake(2, "Plain", Some(1))]);

  // M-N: fillings of those cakes, keyed by the junction column.
  mock
    .expect_select::<filling::Entity>()
    .sql_contains(r#"WHERE ("cake_filling"."cake_id") IN ("#)
    .with_args((1, 2))
    .returning_with_columns([(filling(1, "Ganache"), [("cake_id", 1)]), (filling(2, "Raspberry"), [("cake_id", 1)])]);

  let db = mock.connection().await;
  let found = bakery_details(&db, 1).await.unwrap().unwrap();

  let mut chocolate: cake::ModelEx = cake(1, "Chocolate", Some(1)).into();
  chocolate.fillings = HasMany::Loaded(vec![filling(1, "Ganache").into(), filling(2, "Raspberry").into()]);

  let mut plain: cake::ModelEx = cake(2, "Plain", Some(1)).into();
  plain.fillings = HasMany::Loaded(vec![]);

  let mut expected: bakery::ModelEx = bakery(1, "Main Street").into();
  expected.profile = HasOne::loaded(Some(profile));
  expected.cakes = HasMany::Loaded(vec![chocolate, plain]);

  assert_eq!(found, expected);
}

#[tokio::test]
async fn function_returning_model_ex_not_found() {
  let mock = MockDb::new(DbBackend::Postgres);

  // Nothing found: no relation is loaded, so no other query is sent.
  mock
    .expect_select::<bakery::Entity>()
    .matching_ignoring_limit(bakery::Entity::find_by_id(42).find_also_related(bakery_profile::Entity))
    .returning_rows(Vec::<(bakery::Model, Option<bakery_profile::Model>)>::new());

  let db = mock.connection().await;
  assert_eq!(bakery_details(&db, 42).await.unwrap(), None);
}

/// Code under test: lists bakeries as `ModelEx`, without loading relations.
async fn list_bakeries(db: &DatabaseConnection) -> Result<Vec<bakery::ModelEx>, DbErr> {
  bakery::Entity::load().order_by_asc(bakery::Column::Name).all(db).await
}

#[tokio::test]
async fn select_returning_model_ex() {
  // `Entity::load()` selects columns under aliases (`AS "A_id"`): the mock
  // renames the model's columns to match, whatever the quoting.
  for backend in [DbBackend::Postgres, DbBackend::MySql, DbBackend::Sqlite] {
    let mock = MockDb::new(backend);
    let bakeries: Vec<bakery::ModelEx> = vec![bakery(2, "Empty Shop").into(), bakery(1, "Main Street").into()];

    mock
      .expect_select::<bakery::Entity>()
      .sql_regex(r#"ORDER BY ["`]bakery["`]\.["`]name["`] ASC"#)
      .returning(bakeries.clone());

    let db = mock.connection().await;
    assert_eq!(list_bakeries(&db).await.unwrap(), bakeries, "{backend:?}");
  }
}

#[tokio::test]
async fn select_returning_model_ex_with_relations_loaded_separately() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock.expect_select::<bakery::Entity>().returning::<bakery::ModelEx>([bakery(1, "Main Street").into()]);
  mock.expect_select::<cake::Entity>().with_args((1,)).returning::<cake::ModelEx>([cake(1, "Chocolate", Some(1)).into()]);

  let db = mock.connection().await;
  let found = bakery::Entity::load().with(cake::Entity).all(&db).await.unwrap();

  let mut expected: bakery::ModelEx = bakery(1, "Main Street").into();
  expected.cakes = HasMany::Loaded(vec![cake(1, "Chocolate", Some(1)).into()]);

  assert_eq!(found, vec![expected]);
}

#[tokio::test]
async fn nested_active_model_save() {
  let mock = MockDb::new(DbBackend::Postgres);
  let profile = bakery_profile::Model {
    id: 10,
    bakery_id: 1,
    description: "Open since 1902".into(),
  };

  // Saving a graph runs in a transaction, and each related model is saved
  // in a nested one (a savepoint). Expecting any BEGIN/COMMIT makes all of
  // them checked.
  mock.expect_begin();
  mock.expect_insert::<bakery::Entity>().with_args(("Main Street",)).returning([bakery(1, "Main Street")]);

  // Setting a has-one relation first looks up the existing related model, to
  // replace it.
  mock
    .expect_select::<bakery_profile::Entity>()
    .matching_ignoring_limit(bakery(1, "Main Street").find_related(bakery_profile::Entity))
    .returning::<bakery_profile::Model>([]);

  mock.expect_begin();
  mock.expect_insert::<bakery_profile::Entity>().with_args((1, "Open since 1902")).returning([profile.clone()]);
  mock.expect_commit();

  // The new bakery's key is propagated to its children.
  mock.expect_begin();
  mock.expect_insert::<cake::Entity>().with_args(("Chocolate", 1005, 1)).returning([cake(5, "Chocolate", Some(1))]);
  mock.expect_commit();

  mock.expect_commit();

  let db = mock.connection().await;
  let saved = bakery::ActiveModel::builder()
    .set_name("Main Street")
    .set_profile(bakery_profile::ActiveModel::builder().set_description("Open since 1902"))
    .add_cake(cake::ActiveModel::builder().set_name("Chocolate").set_price_cents(1005))
    .save(&db)
    .await
    .unwrap();

  let saved: bakery::ModelEx = saved.try_into_model().unwrap();

  let mut expected: bakery::ModelEx = bakery(1, "Main Street").into();
  expected.profile = HasOne::loaded(Some(profile));
  expected.cakes = HasMany::Loaded(vec![cake(5, "Chocolate", Some(1)).into()]);

  assert_eq!(saved, expected);
}
