//! Relations: joins, related queries, loaders and junction tables.

mod common;

use common::{problems, schema::*};
use leadline::MockDb;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, DbBackend, EntityTrait, JoinType, LoaderTrait, ModelTrait, QueryFilter, QueryOrder, QuerySelect, RelationTrait};

#[tokio::test]
async fn belongs_to_find_also_related() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock
    .expect_select::<cake::Entity>()
    .matching(cake::Entity::find().find_also_related(bakery::Entity))
    .returning_rows([(cake(1, "Chocolate", Some(1)), Some(bakery(1, "Main Street"))), (cake(2, "Orphan", None), None)]);

  let db = mock.connection().await;
  let found = cake::Entity::find().find_also_related(bakery::Entity).all(&db).await.unwrap();

  assert_eq!(found, vec![(cake(1, "Chocolate", Some(1)), Some(bakery(1, "Main Street"))), (cake(2, "Orphan", None), None),]);
}

#[tokio::test]
async fn has_many_find_with_related_consolidates_rows() {
  let mock = MockDb::new(DbBackend::Postgres);

  // One joined row per (bakery, cake) pair, as the database would return.
  mock.expect_select::<bakery::Entity>().matching(bakery::Entity::find().find_with_related(cake::Entity)).returning_rows([
    (bakery(1, "Main Street"), Some(cake(1, "Chocolate", Some(1)))),
    (bakery(1, "Main Street"), Some(cake(2, "Lemon", Some(1)))),
    (bakery(2, "Empty Shop"), None),
  ]);

  let db = mock.connection().await;
  let found = bakery::Entity::find().find_with_related(cake::Entity).all(&db).await.unwrap();

  assert_eq!(
    found,
    vec![
      (bakery(1, "Main Street"), vec![cake(1, "Chocolate", Some(1)), cake(2, "Lemon", Some(1))]),
      (bakery(2, "Empty Shop"), vec![]),
    ]
  );
}

#[tokio::test]
async fn has_one_find_related_from_model() {
  let mock = MockDb::new(DbBackend::Postgres);
  let main_street = bakery(1, "Main Street");
  let profile = bakery_profile::Model {
    id: 10,
    bakery_id: 1,
    description: "Open since 1902".into(),
  };

  mock
    .expect_select::<bakery_profile::Entity>()
    .matching_ignoring_limit(main_street.find_related(bakery_profile::Entity))
    .returning([profile.clone()]);

  let db = mock.connection().await;
  let found = main_street.find_related(bakery_profile::Entity).one(&db).await.unwrap();

  assert_eq!(found, Some(profile));
}

#[tokio::test]
async fn many_to_many_find_related_through_junction() {
  let mock = MockDb::new(DbBackend::Postgres);
  let chocolate = cake(1, "Chocolate", Some(1));

  mock
    .expect_select::<filling::Entity>()
    .matching(chocolate.find_related(filling::Entity))
    .returning([filling(1, "Ganache"), filling(2, "Raspberry")]);

  let db = mock.connection().await;
  let found = chocolate.find_related(filling::Entity).all(&db).await.unwrap();

  assert_eq!(found, vec![filling(1, "Ganache"), filling(2, "Raspberry")]);

  let sql = &mock.statements()[0].sql;
  assert!(sql.contains(r#"INNER JOIN "cake_filling""#), "{sql}");
}

#[tokio::test]
async fn filter_through_a_join() {
  let mock = MockDb::new(DbBackend::Postgres);

  let query = || {
    cake::Entity::find()
      .join(JoinType::InnerJoin, cake::Relation::Bakery.def())
      .filter(bakery::Column::Name.eq("Main Street"))
      .filter(cake::Column::PriceCents.lt(2000))
      .order_by_asc(cake::Column::Name)
  };

  mock.expect_select::<cake::Entity>().matching(query()).returning([cake(1, "Chocolate", Some(1))]);

  let db = mock.connection().await;
  let found = query().all(&db).await.unwrap();

  assert_eq!(found, vec![cake(1, "Chocolate", Some(1))]);
}

#[tokio::test]
async fn has_many_loader_batches_children() {
  let mock = MockDb::new(DbBackend::Postgres);
  let bakeries = vec![bakery(1, "Main Street"), bakery(2, "Empty Shop"), bakery(3, "Station")];

  mock.expect_select::<cake::Entity>().sql_contains(r#"WHERE ("cake"."bakery_id") IN ("#).with_args((1, 2, 3)).returning([
    cake(1, "Chocolate", Some(1)),
    cake(2, "Lemon", Some(3)),
    cake(3, "Carrot", Some(1)),
  ]);

  let db = mock.connection().await;
  let cakes = bakeries.load_many(cake::Entity, &db).await.unwrap();

  assert_eq!(cakes, vec![vec![cake(1, "Chocolate", Some(1)), cake(3, "Carrot", Some(1))], vec![], vec![cake(2, "Lemon", Some(3))],]);
}

#[tokio::test]
async fn belongs_to_loader() {
  let mock = MockDb::new(DbBackend::Postgres);
  let cakes = vec![cake(1, "Chocolate", Some(1)), cake(2, "Orphan", None), cake(3, "Lemon", Some(1))];

  // The parent is fetched once per distinct key, NULL included.
  mock
    .expect_select::<bakery::Entity>()
    .sql_contains(r#"WHERE ("bakery"."id") IN ("#)
    .with_args((1, None::<i32>))
    .returning([bakery(1, "Main Street")]);

  let db = mock.connection().await;
  let bakeries = cakes.load_one(bakery::Entity, &db).await.unwrap();

  assert_eq!(bakeries, vec![Some(bakery(1, "Main Street")), None, Some(bakery(1, "Main Street"))]);
}

#[tokio::test]
async fn many_to_many_loader() {
  let mock = MockDb::new(DbBackend::Postgres);
  let cakes = vec![cake(1, "Chocolate", Some(1)), cake(2, "Marble", Some(1)), cake(3, "Plain", Some(1))];

  // A single query joins the junction table, and selects its key next to
  // each filling so SeaORM can group them: rows need that extra column.
  // The two conditions could also be chained `sql_contains`: they all apply.
  mock
    .expect_select::<filling::Entity>()
    .sql_regex(r#"INNER JOIN "cake_filling" .* WHERE \("cake_filling"\."cake_id"\) IN \("#)
    .with_args((1, 2, 3))
    .returning_with_columns([
      (filling(1, "Ganache"), [("cake_id", 1)]),
      (filling(1, "Ganache"), [("cake_id", 2)]),
      (filling(2, "Raspberry"), [("cake_id", 1)]),
    ]);

  let db = mock.connection().await;
  let fillings = cakes.load_many(filling::Entity, &db).await.unwrap();

  assert_eq!(fillings, vec![vec![filling(1, "Ganache"), filling(2, "Raspberry")], vec![filling(1, "Ganache")], vec![]]);
}

#[tokio::test]
async fn self_referential_loaders() {
  let mock = MockDb::new(DbBackend::Postgres);
  let ada = baker(1, "Ada", None);
  let grace = baker(2, "Grace", Some(1));
  let linus = baker(3, "Linus", Some(1));
  let bakers = vec![ada.clone(), grace.clone(), linus.clone()];

  // Mentors: the bakers whose id is one of the mentor ids (NULL included).
  mock
    .expect_select::<baker::Entity>()
    .sql_contains(r#"WHERE ("baker"."id") IN ("#)
    .with_args((None::<i32>, 1))
    .returning([ada.clone()]);

  // Mentees: the bakers whose mentor is one of the given bakers.
  mock
    .expect_select::<baker::Entity>()
    .sql_contains(r#"WHERE ("baker"."mentor_id") IN ("#)
    .with_args((1, 2, 3))
    .returning([grace.clone(), linus.clone()]);

  let db = mock.connection().await;

  let mentors = bakers.load_self(baker::Entity, baker::Relation::Mentor, &db).await.unwrap();
  assert_eq!(mentors, vec![None, Some(ada.clone()), Some(ada.clone())]);

  let mentees = bakers.load_self_many(baker::Entity, baker::Relation::Mentor, &db).await.unwrap();
  assert_eq!(mentees, vec![vec![grace, linus], vec![], vec![]]);
}

#[tokio::test]
async fn insert_into_junction_with_composite_key() {
  // Postgres returns the inserted row.
  let mock = MockDb::new(DbBackend::Postgres);
  let link = cake_filling::Model { cake_id: 1, filling_id: 2 };

  mock.expect_insert::<cake_filling::Entity>().with_args((1, 2)).returning([link.clone()]);

  let db = mock.connection().await;
  let inserted = cake_filling::ActiveModel { cake_id: Set(1), filling_id: Set(2) }.insert(&db).await.unwrap();

  assert_eq!(inserted, link);
}

#[tokio::test]
async fn insert_into_junction_with_composite_key_on_mysql() {
  // MySQL has no RETURNING: the row is inserted, then selected back by key.
  let mock = MockDb::new(DbBackend::MySql);
  let link = cake_filling::Model { cake_id: 1, filling_id: 2 };

  mock.expect_insert::<cake_filling::Entity>().with_args((1, 2)).rows_affected(1);
  mock
    .expect_select::<cake_filling::Entity>()
    .matching_ignoring_limit(cake_filling::Entity::find_by_id((1, 2)))
    .returning([link.clone()]);

  let db = mock.connection().await;
  let inserted = cake_filling::ActiveModel { cake_id: Set(1), filling_id: Set(2) }.insert(&db).await.unwrap();

  assert_eq!(inserted, link);
}

#[tokio::test]
async fn typed_expectations_check_the_main_table() {
  let mock = MockDb::new(DbBackend::Postgres);

  // The query mentions `cake`, but selects bakeries.
  mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);

  let db = mock.connection().await;
  let problems = problems(&mock, async move {
    let _ = bakery::Entity::find().inner_join(cake::Entity).filter(cake::Column::Name.eq("Chocolate")).all(&db).await;
  })
  .await;

  assert!(problems[0].contains("it targets `bakery`, not `cake`"), "{problems:?}");
}
