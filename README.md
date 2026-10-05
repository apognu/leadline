# leadline

leadline is a mock database for testing code built on [SeaORM](https://www.sea-ql.org/SeaORM/) 2.0. A test declares the statements the code is expected to send and what each of them returns, then hands the code a `DatabaseConnection`. Any statement that was not expected fails the test.

```rust
use leadline::MockDb;
use sea_orm::{DatabaseConnection, DbBackend, DbErr, EntityTrait, ModelTrait};

// Code under test.
async fn delete_cake(db: &DatabaseConnection, id: i32) -> Result<bool, DbErr> {
  let Some(cake) = cake::Entity::find_by_id(id).one(db).await? else {
    return Ok(false);
  };

  cake.delete(db).await?;

  Ok(true)
}

#[tokio::test]
async fn deletes_the_cake() {
  let mock = MockDb::new(DbBackend::Postgres);

  mock
    .expect_select::<cake::Entity>()
    .matching_ignoring_limit(cake::Entity::find_by_id(1))
    .returning([cake::Model { id: 1, name: "Chocolate".into() }]);

  mock.expect_delete::<cake::Entity>().with_args((1,)).rows_affected(1);

  let db = mock.connection().await;
  assert!(delete_cake(&db, 1).await.unwrap());
}
```

When the mock goes out of scope, it fails the test if any expectation was left unmet.

## Comparison with SeaORM's `MockDatabase`

SeaORM provides its own `MockDatabase`, which returns queued results in order and records the statements it receives, so that a test can inspect them afterwards. It is a good fit when a test mostly needs data to flow through the code.

leadline takes a different approach: it checks each statement when it is received.

- A statement must be of the expected kind (`SELECT`, `INSERT`, …) and, for expectations typed by entity, target the expected table.
- Statements can be matched against the same query builder the code uses, against their SQL, or against their bound values.
- Results are typed: a `SELECT` on `cake::Entity` returns `cake::Model`s, which the compiler enforces.
- When a statement does not match, the test fails immediately, and the message shows what was sent and what was expected.

The mock provides a real `sea_orm::DatabaseConnection`, built on SeaORM's proxy driver. Code taking `&DatabaseConnection`, `impl ConnectionTrait` or `impl TransactionTrait` runs unchanged, including transactions.

## Installation

```toml
[dev-dependencies]
leadline = "0.1"
```

## Writing expectations

Each expectation starts with an `expect_*` method of the mock:

| Method                                                                 | Expects                   | Results                                                   |
| ---------------------------------------------------------------------- | ------------------------- | --------------------------------------------------------- |
| `expect_select::<E>()`                                                 | a `SELECT` on `E`'s table | `E`'s models                                              |
| `expect_insert::<E>()`, `expect_update::<E>()`, `expect_delete::<E>()` | a write on `E`'s table    | affected rows, last insert ID, or models from `RETURNING` |
| `expect_query()`                                                       | any `SELECT`              | raw rows                                                  |
| `expect_statement()`                                                   | any statement             | raw rows or an exec result                                |
| `expect_begin()`, `expect_commit()`, `expect_rollback()`               | transaction boundaries    | none                                                      |

The expectation is then narrowed down with matchers, and completed with a result.

### Matching statements

The most precise matcher takes the query builder the code under test uses. Both the generated SQL and the bound values must be equal:

```rust
mock
  .expect_select::<cake::Entity>()
  .matching(cake::Entity::find().filter(cake::Column::Name.contains("choc")))
  .returning::<cake::Model>([]);
```

Since no SQL is written by hand, these expectations follow refactors well, and a renamed column or a value of the wrong type does not compile.

Some executors modify the query before sending it: `.one()` adds `LIMIT 1`, and paginators add `LIMIT` and `OFFSET`. `matching_ignoring_limit` ignores these clauses and their values:

```rust
mock
  .expect_select::<cake::Entity>()
  .matching_ignoring_limit(cake::Entity::find_by_id(1))
  .returning([chocolate]);
```

When only part of a statement matters, `sql`, `sql_contains` and `sql_regex` check its SQL text, and `with_args` checks its bound values, in order. `Any` accepts any value for one argument:

```rust
use leadline::Any;

mock
  .expect_select::<cake::Entity>()
  .sql_contains(r#"WHERE "cake"."bakery_id" = $1"#)
  .with_args((3,))
  .returning(cakes);

mock.expect_delete::<cake::Entity>().with_args((Any,)).rows_affected(1);
```

Expectations typed by entity also check the table: the statement's main table, after `FROM`, `INTO` or `UPDATE`, must be the entity's table.

### Returning results

Reads return models, either `Model`s or, with SeaORM 2.0's dense entity format, `ModelEx`s. An empty result needs its type to be specified:

```rust
mock.expect_select::<cake::Entity>().returning(vec![chocolate, lemon]);
mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);
```

Writes report affected rows or the last inserted ID. Writes that read rows back through `RETURNING`, such as `ActiveModel::insert` on Postgres, return models instead:

```rust
mock.expect_update::<cake::Entity>().rows_affected(3);
mock.expect_insert::<cake::Entity>().last_insert_id(42);
mock.expect_insert::<cake::Entity>().returning([cake::Model { id: 7, name: "Lemon".into() }]);
```

`returning_error` fails the statement with a `DbErr`, to test how the code handles database errors. Results can also be computed from the incoming statement, or built for queries selecting several entities; the [API documentation](https://docs.rs/leadline) describes these options.

### Order and repetition

Statements must arrive in the order their expectations were declared, unless `mock.unordered()` is called, which is useful for statements sent concurrently. `times(n)` expects a statement several times, and `maybe()` makes an expectation optional.

### Transactions

Transactions are only checked once at least one is expected. From then on, every `BEGIN`, `COMMIT` and `ROLLBACK` must be expected, including those of nested transactions:

```rust
mock.expect_begin();
mock.expect_delete::<cake::Entity>().rows_affected(1);
mock.expect_commit();
```

## Failures

A statement that matches no expectation makes the call panic, which fails the test at that point:

```text
leadline: unexpected SELECT `SELECT "cake"."id", "cake"."name" FROM "cake" WHERE "cake"."id" = $1`
with [Int(Some(2))]: next expectation is SELECT on `cake` with statement `…` with [Int(Some(1))],
but it does not match statement `…`
```

When the last clone of the mock is dropped, it reports any remaining problem: unmet expectations, and unexpected statements whose panic was caught elsewhere, for example in a spawned task. `mock.verify()` performs the same check during a test, and `mock.check()` returns the problems instead of panicking.

## Things to keep in mind

The same SeaORM call can send different statements depending on the backend. For example, `ActiveModel::insert` is a single `INSERT … RETURNING` on Postgres, but an `INSERT` followed by a `SELECT` on MySQL. Tests should use the backend the code runs against in production.

SeaORM occasionally sends statements that are not obvious from the code, such as the queries of entity loaders or nested saves. The failure message always includes the SQL that was actually sent, which is the simplest way to find out what to expect. `mock.statements()` also lists every statement received so far.

Rows built from models are renamed to the aliases a query selects, including through common table expressions and subqueries, as long as these keep column names. A column renamed inside a CTE or subquery is not traced back to the model; such rows can be written out with `returning_rows`.

## Documentation

The full API, with examples for every method, is documented on [docs.rs](https://docs.rs/leadline).
