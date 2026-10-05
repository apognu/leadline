use std::{collections::BTreeMap, marker::PhantomData, sync::Arc};

use regex::Regex;
use sea_orm::{
  DbBackend, DbErr, EntityTrait, Iden, IntoMockRow, Iterable, ModelTrait, PrimaryKeyToColumn, PrimaryKeyTrait, ProxyExecResult, ProxyRow, QueryTrait, Statement, StatementBuilder, TryFromU64, Value,
  sea_query::IntoValueTuple,
};

use crate::{
  expectation::{Exec, Expectation, Rows, into_proxy_row},
  matcher::{IntoArgs, SqlMatcher, integer},
  mock::ExpectationRef,
};

/// The type of the primary key of `E`: an integer, a `Uuid`, a `String`, a
/// tuple for composite keys, …
type KeyOf<E> = <<E as EntityTrait>::PrimaryKey as PrimaryKeyTrait>::ValueType;

/// The primary key of `model` as a `u64`, if it is a single integer column.
///
/// On MySQL, which has no `RETURNING`, SeaORM learns the key of an inserted row
/// from the "last inserted ID", so the mock reports this key as that ID. A
/// `cake` with `id: 5` gives `Some(5)`; a key that is a `Uuid`, or made of
/// several columns, gives `None`.
fn integer_key<M: ModelTrait>(model: &M) -> Option<u64> {
  let mut keys = <M::Entity as EntityTrait>::PrimaryKey::iter();

  let (Some(key), None) = (keys.next(), keys.next()) else {
    return None;
  };

  integer(&model.get(key.into_column()))?.and_then(|n| u64::try_from(n).ok())
}

/// Methods shared by the three pending builders: the matchers, `times`, `maybe`
/// and `returning_error`.
///
/// Matchers add up: `.sql_contains("FROM").sql_contains("ORDER BY")` only
/// accepts statements containing both.
macro_rules! matching_methods {
  () => {
    /// Accept only the statement that `query` builds: the same SQL, and the
    /// same bound values.
    ///
    /// This is the most precise matcher, and the most robust one: the expected
    /// statement comes from the same query builder as the code under test, so a
    /// renamed column or a value of the wrong type is a compile error, and no
    /// SQL is written by hand. The query is built for the mock's backend.
    ///
    /// Some SeaORM methods add a `LIMIT` to the query they send:
    /// `find_by_id(1).one(db)` sends `… WHERE "id" = $1 LIMIT $2`, which
    /// `matching(find_by_id(1))` rejects. To expect such a query, use
    /// [`matching_ignoring_limit`](Self::matching_ignoring_limit). When a
    /// statement only differs by its `LIMIT` or `OFFSET`, the failure message
    /// suggests it.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{ColumnTrait, DbBackend, EntityTrait, QueryFilter};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    /// let query = || cake::Entity::find().filter(cake::Column::Name.contains("choc"));
    ///
    /// mock.expect_select::<cake::Entity>().matching(query()).returning::<cake::Model>([]);
    ///
    /// let db = mock.connection().await;
    /// assert!(query().all(&db).await.unwrap().is_empty());
    /// # }
    /// ```
    pub fn matching<Q: QueryTrait>(self, query: Q) -> Self {
      let stmt = query.build(self.backend);
      self.matching_statement(stmt)
    }

    /// Like [`matching`](Self::matching), but ignores `LIMIT` and `OFFSET`, and
    /// the values bound to them, in both statements.
    ///
    /// `.one()` adds a `LIMIT`, and paginators add both. This lets the expected
    /// query be written as the code under test builds it:
    ///
    /// ```text
    /// expected   find_by_id(1)           SELECT … WHERE "id" = $1             [1]
    /// sent       find_by_id(1).one(db)   SELECT … WHERE "id" = $1 LIMIT $2    [1, 1]
    /// compared                           SELECT … WHERE "id" = $1             [1]
    /// ```
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{DbBackend, EntityTrait};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock
    ///   .expect_select::<cake::Entity>()
    ///   .matching_ignoring_limit(cake::Entity::find_by_id(1))
    ///   .returning([cake::Model { id: 1, name: "Chocolate".into(), bakery_id: None }]);
    ///
    /// let db = mock.connection().await;
    /// let found = cake::Entity::find_by_id(1).one(&db).await.unwrap();
    ///
    /// assert_eq!(found.unwrap().name, "Chocolate");
    /// # }
    /// ```
    pub fn matching_ignoring_limit<Q: QueryTrait>(self, query: Q) -> Self {
      let stmt = query.build(self.backend);
      self.set(|e| e.spec_mut().matchers.push(SqlMatcher::statement(stmt, true)))
    }

    /// Like [`matching`](Self::matching), for a statement built with
    /// `sea_query` (`Query::select()`, …) rather than with SeaORM's entities.
    ///
    /// The statement is built for the mock's backend, so that the test does not
    /// have to name the backend again: on a Postgres mock,
    /// `matching_query(&query)` is the same as
    /// `matching_statement(DbBackend::Postgres.build(&query))`.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{ConnectionTrait, DbBackend, sea_query::{Expr, ExprTrait, Query}};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    /// let query = Query::select().column("name").from("cake").and_where(Expr::col("id").eq(1)).to_owned();
    ///
    /// mock.expect_query().matching_query(&query).returning_rows(Vec::<std::collections::BTreeMap<String, sea_orm::Value>>::new());
    ///
    /// let db = mock.connection().await;
    /// assert!(db.query_one(&query).await.unwrap().is_none());
    /// # }
    /// ```
    pub fn matching_query<S: StatementBuilder>(self, query: &S) -> Self {
      let stmt = self.backend.build(query);
      self.matching_statement(stmt)
    }

    /// Accept only `stmt`: the same SQL, ignoring formatting, and the same
    /// bound values.
    ///
    /// For statements that are already built, such as raw `Statement`s. Prefer
    /// [`matching`](Self::matching) for SeaORM queries, and
    /// [`matching_query`](Self::matching_query) for `sea_query` statements:
    /// both build the statement for the mock's backend.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{ConnectionTrait, DbBackend, Statement};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    /// let stmt = || Statement::from_sql_and_values(DbBackend::Postgres, r#"DELETE FROM "cake" WHERE "id" = $1"#, [1.into()]);
    ///
    /// mock.expect_statement().matching_statement(stmt()).rows_affected(1);
    ///
    /// let db = mock.connection().await;
    /// assert_eq!(db.execute_raw(stmt()).await.unwrap().rows_affected(), 1);
    /// # }
    /// ```
    pub fn matching_statement(self, stmt: Statement) -> Self {
      self.set(|e| e.spec_mut().matchers.push(SqlMatcher::statement(stmt, false)))
    }

    /// Accept statements whose SQL is `sql`, ignoring formatting: `SELECT  *
    /// FROM t` (on two lines) matches `SELECT * FROM t`. Only the whitespace
    /// between tokens is ignored: string literals, quoted identifiers and
    /// comments must be identical.
    ///
    /// Bound values are not checked: add [`with_args`](Self::with_args) for
    /// them. For statements built by SeaORM, prefer
    /// [`matching`](Self::matching), as the SQL SeaORM generates may change
    /// between versions.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use std::collections::BTreeMap;
    ///
    /// use sea_orm::{ConnectionTrait, DbBackend, Statement, Value};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock.expect_query().sql("SELECT  1 AS one").returning_rows([BTreeMap::from([("one", Value::from(1))])]);
    ///
    /// let db = mock.connection().await;
    /// let row = db.query_one_raw(Statement::from_string(DbBackend::Postgres, "SELECT 1 AS one")).await.unwrap();
    ///
    /// assert_eq!(row.unwrap().try_get::<i32>("", "one").unwrap(), 1);
    /// # }
    /// ```
    pub fn sql(self, sql: impl Into<String>) -> Self {
      let sql = sql.into();
      self.set(|e| e.spec_mut().matchers.push(SqlMatcher::Exact(sql)))
    }

    /// Accept statements whose SQL matches the regular expression `pattern`,
    /// anywhere in the SQL unless the pattern is anchored with `^` or `$`. The
    /// SQL is matched exactly as it was sent, formatting included.
    ///
    /// Useful when only part of a statement matters, or when part of the
    /// statement varies, such as the `IN (…)` lists that loaders generate.
    ///
    /// # Panics
    ///
    /// If `pattern` is not a valid regular expression.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{DbBackend, EntityTrait, QueryOrder};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock.expect_select::<cake::Entity>().sql_regex(r#"ORDER BY "cake"\."name" (ASC|DESC)"#).returning::<cake::Model>([]);
    ///
    /// let db = mock.connection().await;
    /// cake::Entity::find().order_by_desc(cake::Column::Name).all(&db).await.unwrap();
    /// # }
    /// ```
    pub fn sql_regex(self, pattern: &str) -> Self {
      let re = Regex::new(pattern).expect("invalid regular expression");
      self.set(|e| e.spec_mut().matchers.push(SqlMatcher::Regex(re)))
    }

    /// Accept statements whose SQL contains `needle`, ignoring formatting as
    /// [`sql`](Self::sql) does: `sql_contains(r#"ORDER BY "name""#)` matches
    /// `SELECT … ORDER BY "name" ASC`.
    ///
    /// The simplest way to check one part of a statement. Chain several
    /// matchers to check several parts: a statement must satisfy all of them.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{ColumnTrait, DbBackend, EntityTrait, QueryFilter, QueryOrder};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock
    ///   .expect_select::<cake::Entity>()
    ///   .sql_contains(r#"WHERE "cake"."bakery_id" = $1"#)
    ///   .sql_contains("ORDER BY")
    ///   .with_args((3,))
    ///   .returning::<cake::Model>([]);
    ///
    /// let db = mock.connection().await;
    /// cake::Entity::find()
    ///   .filter(cake::Column::BakeryId.eq(3))
    ///   .order_by_asc(cake::Column::Id)
    ///   .all(&db)
    ///   .await
    ///   .unwrap();
    /// # }
    /// ```
    pub fn sql_contains(self, needle: impl Into<String>) -> Self {
      let needle = needle.into();
      self.set(|e| e.spec_mut().matchers.push(SqlMatcher::Contains(needle)))
    }

    /// Accept statements for which `predicate` returns `true`.
    ///
    /// For conditions no other matcher expresses. The predicate sees the
    /// whole [`Statement`]: SQL, bound values and backend.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{DbBackend, EntityTrait};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock
    ///   .expect_select::<cake::Entity>()
    ///   .sql_fn(|stmt| !stmt.sql.contains("LIMIT"))
    ///   .returning::<cake::Model>([]);
    ///
    /// let db = mock.connection().await;
    /// cake::Entity::find().all(&db).await.unwrap();
    /// # }
    /// ```
    pub fn sql_fn(self, predicate: impl Fn(&Statement) -> bool + Send + Sync + 'static) -> Self {
      self.set(|e| e.spec_mut().matchers.push(SqlMatcher::Fn(Arc::new(predicate))))
    }

    /// Accept only statements whose bound values match `args`, one by one and
    /// in order. The statement must have exactly as many values.
    ///
    /// `args` is a tuple, or a `Vec`, with one entry per value:
    ///
    /// - a plain value matches an equal value: `(1, "Lemon")`;
    /// - [`Any`](crate::Any) matches any value: `(Any, "Lemon")`;
    /// - [`Arg::matching`](crate::Arg::matching) runs a custom check.
    ///
    /// Integers compare by value, whatever their width: `1` is bound as an
    /// `i32`, but matches the `1` of a `BigInt` column. Other values must have
    /// the same type: a `String` does not match a `Char`, nor an `f32` an
    /// `f64`.
    ///
    /// Values are checked independently of the SQL, which makes this a good
    /// companion to the `sql*` matchers.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{ColumnTrait, DbBackend, EntityTrait, QueryFilter, Value, sea_query::Expr};
    /// use leadline::{Any, Arg, MockDb};
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock
    ///   .expect_update::<cake::Entity>()
    ///   .with_args(("Lemon", Arg::matching(|v| matches!(v, Value::Int(Some(id)) if *id > 10))))
    ///   .rows_affected(1);
    /// mock.expect_delete::<cake::Entity>().with_args((Any,)).rows_affected(1);
    ///
    /// let db = mock.connection().await;
    /// cake::Entity::update_many()
    ///   .col_expr(cake::Column::Name, Expr::value("Lemon"))
    ///   .filter(cake::Column::Id.eq(42))
    ///   .exec(&db)
    ///   .await
    ///   .unwrap();
    /// cake::Entity::delete_by_id(7).exec(&db).await.unwrap();
    /// # }
    /// ```
    pub fn with_args(self, args: impl IntoArgs) -> Self {
      let args = args.into_args();
      self.set(|e| e.spec_mut().args = Some(args))
    }

    /// Expect this statement exactly `n` times, or up to `n` times when the
    /// expectation is also [`maybe`](Self::maybe). The default is once.
    ///
    /// This saves declaring the same expectation several times, for
    /// instance for a loop. Every call gets the same result, unless it is
    /// computed by a closure (`returning_with`, `exec_with`).
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{DbBackend, EntityTrait};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock.expect_delete::<cake::Entity>().times(3).rows_affected(1);
    ///
    /// let db = mock.connection().await;
    /// for id in 1..=3 {
    ///   cake::Entity::delete_by_id(id).exec(&db).await.unwrap();
    /// }
    /// # }
    /// ```
    pub fn times(self, n: usize) -> Self {
      self.set(|e| {
        e.min = if e.min == 0 { 0 } else { n };
        e.max = n;
      })
    }

    /// Make this expectation optional: it may be met up to its
    /// [`times`](Self::times) (once by default), or not at all.
    ///
    /// For statements the code may or may not send, such as a lookup skipped
    /// when a cache is warm. In an ordered mock, an optional expectation is
    /// skipped as soon as a later one matches, and cannot be met afterwards:
    /// with an optional `SELECT` expected before a `DELETE`, receiving the
    /// `DELETE` first skips the `SELECT` for good.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{DbBackend, EntityTrait};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock.expect_select::<cake::Entity>().maybe().returning::<cake::Model>([]);
    /// mock.expect_delete::<cake::Entity>().rows_affected(1);
    ///
    /// let db = mock.connection().await;
    /// // The SELECT is never sent: the expectation is skipped.
    /// cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
    /// # }
    /// ```
    pub fn maybe(self) -> Self {
      self.set(|e| e.min = 0)
    }

    /// Complete the expectation by failing the statement with `err`.
    ///
    /// For testing how the code under test handles database errors:
    /// constraint violations, lost connections, and so on.
    ///
    /// ```
    /// # include!("../doctests/entities.rs");
    /// use sea_orm::{DbBackend, DbErr, EntityTrait};
    /// use leadline::MockDb;
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let mock = MockDb::new(DbBackend::Postgres);
    ///
    /// mock
    ///   .expect_delete::<cake::Entity>()
    ///   .returning_error(DbErr::Custom("foreign key violation".into()));
    ///
    /// let db = mock.connection().await;
    /// let err = cake::Entity::delete_by_id(1).exec(&db).await.unwrap_err();
    ///
    /// assert!(err.to_string().contains("foreign key violation"));
    /// # }
    /// ```
    pub fn returning_error(self, err: DbErr) {
      self.inner.update(|e| e.response_mut().error = Some(err));
    }

    fn set(self, f: impl FnOnce(&mut Expectation)) -> Self {
      self.inner.update(f);
      self
    }
  };
}

/// Pending expectation for a `SELECT` on entity `E`, created by
/// [`MockDb::expect_select`](crate::MockDb::expect_select).
///
/// Narrow the statements it accepts with the matching methods, then complete it
/// with one result: [`returning`](Self::returning),
/// [`returning_with`](Self::returning_with),
/// [`returning_with_columns`](Self::returning_with_columns),
/// [`returning_rows`](Self::returning_rows) or
/// [`returning_error`](Self::returning_error). The result methods consume the
/// builder, so an expectation cannot get two results, and forgetting the result
/// is a compiler warning.
///
/// Being typed by entity, it only accepts statements on `E`'s table, and
/// returns `E`'s models.
#[must_use = "an expectation needs a result: call `returning(..)`, `returning_with(..)`, `returning_rows(..)` or `returning_error(..)`"]
pub struct SelectExpectation<E: EntityTrait> {
  inner: ExpectationRef,
  backend: DbBackend,
  _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> SelectExpectation<E> {
  pub(crate) fn new(inner: ExpectationRef, backend: DbBackend) -> Self {
    Self { inner, backend, _entity: PhantomData }
  }

  matching_methods!();

  /// Complete the expectation by returning these models: `E::Model`s, or
  /// `E::ModelEx`s with SeaORM 2.0's dense entity format.
  ///
  /// Models keep the result checked at compile time: a model of another entity
  /// is rejected. An empty result needs the model type:
  /// `returning::<cake::Model>([])`.
  ///
  /// Rows follow the columns the query selects, aliases included. For `SELECT
  /// "cake"."id" AS "A_id" FROM "cake"`, as `Entity::load()` sends, a `cake`
  /// with `id: 1` is returned as the row `A_id = 1`. Aliases are followed
  /// through table aliases, common table expressions and subqueries, but not
  /// through a column renamed inside them: in `WITH c AS (SELECT id AS inner_id
  /// FROM cake) SELECT c.inner_id AS value FROM c`, `value` is missing. Write
  /// such rows out with [`returning_rows`](Self::returning_rows).
  ///
  /// Only the models' own columns are returned: relations loaded in a `ModelEx`
  /// are ignored, because each relation is loaded by its own query, which needs
  /// its own expectation.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// let cakes = vec![
  ///   cake::Model { id: 1, name: "Chocolate".into(), bakery_id: Some(1) },
  ///   cake::Model { id: 2, name: "Lemon".into(), bakery_id: None },
  /// ];
  ///
  /// mock.expect_select::<cake::Entity>().returning(cakes.clone());
  /// mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);
  ///
  /// let db = mock.connection().await;
  /// assert_eq!(cake::Entity::find().all(&db).await.unwrap(), cakes);
  /// assert!(cake::Entity::find().all(&db).await.unwrap().is_empty());
  /// # }
  /// ```
  pub fn returning<M>(self, models: impl IntoIterator<Item = M>)
  where
    M: ModelTrait<Entity = E>,
  {
    let rows = models.into_iter().map(into_proxy_row).collect();
    self.inner.update(|e| e.response_mut().rows = Some(Rows::Static(rows)));
  }

  /// Complete the expectation by computing the returned models from the
  /// incoming statement, each time it is received.
  ///
  /// For results that depend on the bound values, or that change between calls
  /// of an expectation used [`times`](Self::times). Returning an `Err` fails the
  /// statement with it.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, DbErr, EntityTrait};
  /// use leadline::{MockDb, StatementExt};
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_select::<cake::Entity>().times(2).returning_with(|stmt| {
  ///   let Some(id) = stmt.arg::<i32>(0) else {
  ///     return Err(DbErr::Custom("expected an id".into()));
  ///   };
  ///
  ///   Ok(vec![cake::Model { id, name: format!("Cake #{id}"), bakery_id: None }])
  /// });
  ///
  /// let db = mock.connection().await;
  /// for id in [3, 4] {
  ///   let found = cake::Entity::find_by_id(id).one(&db).await.unwrap();
  ///   assert_eq!(found.unwrap().name, format!("Cake #{id}"));
  /// }
  /// # }
  /// ```
  pub fn returning_with<M, F>(self, f: F)
  where
    M: ModelTrait<Entity = E>,
    F: Fn(&Statement) -> Result<Vec<M>, DbErr> + Send + Sync + 'static,
  {
    let f = move |stmt: &Statement| Ok(f(stmt)?.into_iter().map(into_proxy_row).collect());
    self.inner.update(|e| e.response_mut().rows = Some(Rows::Fn(Arc::new(f))));
  }

  /// Complete the expectation by returning these models, each with extra
  /// columns next to its own.
  ///
  /// Some queries select more than the entity's columns, and SeaORM reads the
  /// extra ones back: computed values, or the junction key that many-to-many
  /// loaders select to group results. A row built from the model alone lacks
  /// them, and decoding fails. `[(ganache, [("cake_id", 1)])]` returns the
  /// columns of the `ganache` model, plus `cake_id = 1`.
  ///
  /// # Panics
  ///
  /// If an extra column has the name of a column of the model, which would
  /// silently change the model's data.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, QuerySelect, sea_query::Expr};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// let chocolate = cake::Model { id: 1, name: "Chocolate".into(), bakery_id: None };
  ///
  /// mock.expect_select::<cake::Entity>().returning_with_columns([(chocolate, [("rank", 3)])]);
  ///
  /// let db = mock.connection().await;
  /// let row = cake::Entity::find()
  ///   .column_as(Expr::cust("RANK() OVER (ORDER BY name)"), "rank")
  ///   .into_json()
  ///   .one(&db)
  ///   .await
  ///   .unwrap()
  ///   .unwrap();
  ///
  /// assert_eq!(row["rank"], 3);
  /// # }
  /// ```
  #[track_caller]
  pub fn returning_with_columns<M, C, K, V>(self, rows: impl IntoIterator<Item = (M, C)>)
  where
    M: ModelTrait<Entity = E>,
    C: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<Value>,
  {
    let mut proxy_rows = Vec::new();

    for (model, columns) in rows {
      let mut row = into_proxy_row(model);

      for (column, value) in columns {
        let column = column.into();

        if row.values.contains_key(&column) {
          panic!("extra column `{column}` is already a column of the model");
        }

        row.values.insert(column, value.into());
      }

      proxy_rows.push(row);
    }

    self.inner.update(|e| e.response_mut().rows = Some(Rows::Static(proxy_rows)));
  }

  /// Complete the expectation by returning rows that are not plain models of
  /// `E`:
  ///
  /// - tuples `(E::Model, Option<F::Model>)` or `(E::Model, F::Model)`, for
  ///   queries selecting two entities, such as `find_also_related` or
  ///   `find_with_related`. The columns of each entity are prefixed: `A_id`,
  ///   `A_name`, `B_id`, …;
  /// - `BTreeMap<String, Value>`s for anything else, with every column written
  ///   out.
  ///
  /// These rows are returned exactly as written: they are not checked against
  /// the entity, nor renamed to the aliases the query selects, unlike the
  /// models given to [`returning`](Self::returning).
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// let main_street = bakery::Model { id: 1, name: "Main Street".into() };
  /// let chocolate = cake::Model { id: 1, name: "Chocolate".into(), bakery_id: Some(1) };
  ///
  /// mock
  ///   .expect_select::<cake::Entity>()
  ///   .returning_rows([(chocolate.clone(), Some(main_street.clone()))]);
  ///
  /// let db = mock.connection().await;
  /// let found = cake::Entity::find().find_also_related(bakery::Entity).all(&db).await.unwrap();
  ///
  /// assert_eq!(found, vec![(chocolate, Some(main_street))]);
  /// # }
  /// ```
  pub fn returning_rows<R: IntoMockRow>(self, rows: impl IntoIterator<Item = R>) {
    let rows = rows.into_iter().map(into_proxy_row).collect();
    self.inner.update(|e| {
      let response = e.response_mut();
      response.rows = Some(Rows::Static(rows));
      response.raw_rows = true;
    });
  }
}

/// Pending expectation for an `INSERT`, `UPDATE` or `DELETE` on entity `E`,
/// created by [`MockDb::expect_insert`](crate::MockDb::expect_insert),
/// [`MockDb::expect_update`](crate::MockDb::expect_update) or
/// [`MockDb::expect_delete`](crate::MockDb::expect_delete).
///
/// Narrow the statements it accepts with the matching methods, then complete it
/// with a result: [`rows_affected`](Self::rows_affected),
/// [`last_insert_id`](Self::last_insert_id),
/// [`last_insert_key`](Self::last_insert_key), [`returning`](Self::returning),
/// [`returning_with`](Self::returning_with), [`exec_with`](Self::exec_with) or
/// [`returning_error`](Self::returning_error). Forgetting the result is a
/// compiler warning.
///
/// Depending on the backend, the same write is either executed, reporting
/// affected rows and the last inserted ID, or sent with `RETURNING`, reading
/// rows back. Each result above answers both.
#[must_use = "an expectation needs a result: call `rows_affected(..)`, `last_insert_id(..)`, `returning(..)`, `exec_with(..)` or `returning_error(..)`"]
pub struct ExecExpectation<E: EntityTrait> {
  inner: ExpectationRef,
  backend: DbBackend,
  _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> ExecExpectation<E> {
  pub(crate) fn new(inner: ExpectationRef, backend: DbBackend) -> Self {
    Self { inner, backend, _entity: PhantomData }
  }

  matching_methods!();

  /// Complete the expectation by reporting `n` affected rows.
  ///
  /// The usual result of updates and deletes. The returned [`ExecResponse`] can
  /// also report a last inserted ID.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, sea_query::Expr};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_update::<cake::Entity>().rows_affected(3);
  ///
  /// let db = mock.connection().await;
  /// let res = cake::Entity::update_many()
  ///   .col_expr(cake::Column::BakeryId, Expr::value(Option::<i32>::None))
  ///   .exec(&db)
  ///   .await
  ///   .unwrap();
  ///
  /// assert_eq!(res.rows_affected, 3);
  /// # }
  /// ```
  pub fn rows_affected(self, n: u64) -> ExecResponse<E> {
    ExecResponse::new(self.inner).rows_affected(n)
  }

  /// Complete the expectation by reporting `id` as the last inserted ID, with
  /// one affected row unless [`ExecResponse::rows_affected`] says otherwise.
  ///
  /// The usual result of inserts, on every backend. MySQL reports the ID
  /// itself. On backends using `RETURNING` (Postgres, SQLite), SeaORM reads the
  /// new key from a returned row instead, so a row holding `id` as the primary
  /// key is returned.
  ///
  /// # Panics
  ///
  /// If the primary key of `E` is not an integer, such as a `Uuid` or a key
  /// made of several columns: use [`last_insert_key`](Self::last_insert_key)
  /// for those.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, Set};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// for backend in [DbBackend::Postgres, DbBackend::MySql] {
  ///   let mock = MockDb::new(backend);
  ///
  ///   mock.expect_insert::<cake::Entity>().last_insert_id(42);
  ///
  ///   let db = mock.connection().await;
  ///   let res = cake::Entity::insert(cake::ActiveModel { name: Set("Lemon".into()), ..Default::default() })
  ///     .exec(&db)
  ///     .await
  ///     .unwrap();
  ///
  ///   assert_eq!(res.last_insert_id, 42);
  /// }
  /// # }
  /// ```
  pub fn last_insert_id(self, id: u64) -> ExecResponse<E> {
    self.inner.update(|e| e.exec_mut().rows_affected = 1);

    ExecResponse::new(self.inner).last_insert_id(id)
  }

  /// Complete the expectation by reporting `key` as the primary key of the
  /// inserted row, with one affected row unless [`ExecResponse::rows_affected`]
  /// says otherwise.
  ///
  /// Like [`last_insert_id`](Self::last_insert_id), for any primary key, in the
  /// entity's own key type: a `Uuid`, a `String`, or a tuple for a key made of
  /// several columns. On backends using `RETURNING` (Postgres, SQLite), a row
  /// holding the key is returned. On MySQL, an integer key is also reported as
  /// the last inserted ID; other keys are chosen by the code before inserting,
  /// so SeaORM does not read them back.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, Set, prelude::Uuid};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let id = Uuid::from_u128(42);
  ///
  /// for backend in [DbBackend::Postgres, DbBackend::MySql] {
  ///   let mock = MockDb::new(backend);
  ///
  ///   mock.expect_insert::<account::Entity>().last_insert_key(id);
  ///
  ///   let db = mock.connection().await;
  ///   let res = account::Entity::insert(account::ActiveModel { id: Set(id), name: Set("Alice".into()) })
  ///     .exec(&db)
  ///     .await
  ///     .unwrap();
  ///
  ///   assert_eq!(res.last_insert_id, id);
  /// }
  /// # }
  /// ```
  pub fn last_insert_key(self, key: impl Into<KeyOf<E>>) -> ExecResponse<E> {
    self.inner.update(|e| e.exec_mut().rows_affected = 1);

    ExecResponse::new(self.inner).last_insert_key(key)
  }

  /// Complete the expectation by returning these models from a `RETURNING`
  /// clause, and reporting them as the affected rows. Like
  /// [`SelectExpectation::returning`], it takes `E::Model`s or `E::ModelEx`s.
  ///
  /// On Postgres and SQLite, SeaORM reads written rows back with `RETURNING`:
  /// `ActiveModel::insert` and `ActiveModel::update` return these models.
  /// On MySQL, which has no `RETURNING`, the write is executed instead, and the
  /// last model's primary key, if it is an integer, is reported as the last
  /// inserted ID.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ActiveModelTrait, DbBackend, Set};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock
  ///   .expect_insert::<cake::Entity>()
  ///   .with_args(("Lemon",))
  ///   .returning([cake::Model { id: 7, name: "Lemon".into(), bakery_id: None }]);
  ///
  /// let db = mock.connection().await;
  /// let inserted = cake::ActiveModel { name: Set("Lemon".into()), ..Default::default() }.insert(&db).await.unwrap();
  ///
  /// assert_eq!(inserted.id, 7);
  /// # }
  /// ```
  pub fn returning<M>(self, models: impl IntoIterator<Item = M>)
  where
    M: ModelTrait<Entity = E>,
  {
    let models: Vec<M> = models.into_iter().collect();
    let last_id = models.last().and_then(integer_key);
    let rows: Vec<_> = models.into_iter().map(into_proxy_row).collect();
    let count = rows.len() as u64;

    self.inner.update(|e| {
      e.response_mut().rows = Some(Rows::Static(rows));
      e.exec_mut().rows_affected = count;

      if let Some(id) = last_id {
        e.exec_mut().last_insert_id = id;
      }
    });
  }

  /// Complete the expectation by computing the models it returns from the
  /// incoming statement, each time one is received.
  ///
  /// Typically to echo back the written values, so that the test does not
  /// repeat them. As with [`returning`](Self::returning), on MySQL the models
  /// are reported as the affected rows, and the last one's integer key as the
  /// last inserted ID. Returning an `Err` fails the statement with it.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ActiveModelTrait, DbBackend, DbErr, Set};
  /// use leadline::{MockDb, StatementExt};
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_insert::<cake::Entity>().returning_with(|stmt| {
  ///   let Some(name) = stmt.arg::<String>(0) else {
  ///     return Err(DbErr::Custom("expected a name".into()));
  ///   };
  ///
  ///   Ok(vec![cake::Model { id: 1, name, bakery_id: None }])
  /// });
  ///
  /// let db = mock.connection().await;
  /// let inserted = cake::ActiveModel { name: Set("Carrot".into()), ..Default::default() }.insert(&db).await.unwrap();
  ///
  /// assert_eq!(inserted.name, "Carrot");
  /// # }
  /// ```
  pub fn returning_with<M, F>(self, f: F)
  where
    M: ModelTrait<Entity = E>,
    F: Fn(&Statement) -> Result<Vec<M>, DbErr> + Send + Sync + 'static,
  {
    let f = Arc::new(f);

    let rows = {
      let f = f.clone();
      move |stmt: &Statement| Ok(f(stmt)?.into_iter().map(into_proxy_row).collect())
    };

    // Without `RETURNING` (MySQL), the write is executed: report the models as
    // the affected rows, and the last one's integer key as the last inserted
    // ID, as `returning` does.
    let exec = move |stmt: &Statement| {
      let models = f(stmt)?;
      let mut result = ProxyExecResult::new(0, models.len() as u64);

      if let Some(id) = models.last().and_then(integer_key) {
        result.last_insert_id = id;
      }

      Ok(result)
    };

    self.inner.update(|e| {
      let response = e.response_mut();
      response.rows = Some(Rows::Fn(Arc::new(rows)));
      response.exec = Some(Exec::Fn(Arc::new(exec)));
    });
  }

  /// Complete the expectation by computing the exec result (affected rows,
  /// last inserted ID) from the incoming statement.
  ///
  /// For results that depend on the bound values, such as the number of IDs in
  /// an `IN` list. Returning an `Err` fails the statement with it.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ColumnTrait, DbBackend, EntityTrait, ProxyExecResult, QueryFilter};
  /// use leadline::{MockDb, StatementExt};
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_delete::<cake::Entity>().exec_with(|stmt| {
  ///   let ids = stmt.args().len();
  ///   Ok(ProxyExecResult::new(0, ids as u64))
  /// });
  ///
  /// let db = mock.connection().await;
  /// let res = cake::Entity::delete_many().filter(cake::Column::Id.is_in([1, 2, 3])).exec(&db).await.unwrap();
  ///
  /// assert_eq!(res.rows_affected, 3);
  /// # }
  /// ```
  pub fn exec_with<F>(self, f: F)
  where
    F: Fn(&Statement) -> Result<ProxyExecResult, DbErr> + Send + Sync + 'static,
  {
    self.inner.update(|e| e.response_mut().exec = Some(Exec::Fn(Arc::new(f))));
  }
}

/// What [`ExecExpectation::rows_affected`],
/// [`last_insert_id`](ExecExpectation::last_insert_id) and
/// [`last_insert_key`](ExecExpectation::last_insert_key) return.
///
/// The expectation is already complete. This only lets the rest of its result
/// be set too, in any order: `.last_insert_id(42).rows_affected(2)`.
pub struct ExecResponse<E: EntityTrait> {
  inner: ExpectationRef,
  _entity: PhantomData<fn() -> E>,
}

impl<E: EntityTrait> ExecResponse<E> {
  fn new(inner: ExpectationRef) -> Self {
    Self { inner, _entity: PhantomData }
  }

  /// Report `n` affected rows, e.g. for a multi-row insert also reporting
  /// its last inserted ID.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, Set};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::MySql);
  ///
  /// mock.expect_insert::<cake::Entity>().last_insert_id(42).rows_affected(2);
  ///
  /// let db = mock.connection().await;
  /// let names = ["Lemon", "Carrot"].map(|name| cake::ActiveModel { name: Set(name.into()), ..Default::default() });
  /// let inserted = cake::Entity::insert_many(names).exec_without_returning(&db).await.unwrap();
  ///
  /// assert_eq!(inserted, 2);
  /// # }
  /// ```
  pub fn rows_affected(self, n: u64) -> Self {
    self.inner.update(|e| e.exec_mut().rows_affected = n);
    self
  }

  /// Report `id` as the last inserted ID, as [`ExecExpectation::last_insert_id`]
  /// does, but keeping the number of affected rows already set.
  ///
  /// # Panics
  ///
  /// If the primary key of `E` cannot be built from a `u64`, such as a `Uuid`:
  /// use [`last_insert_key`](Self::last_insert_key) for such keys.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, Set};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::MySql);
  ///
  /// mock.expect_insert::<cake::Entity>().rows_affected(1).last_insert_id(42);
  ///
  /// let db = mock.connection().await;
  /// let res = cake::Entity::insert(cake::ActiveModel { name: Set("Lemon".into()), ..Default::default() })
  ///   .exec(&db)
  ///   .await
  ///   .unwrap();
  ///
  /// assert_eq!(res.last_insert_id, 42);
  /// # }
  /// ```
  pub fn last_insert_id(self, id: u64) -> Self {
    let key = <KeyOf<E> as TryFromU64>::try_from_u64(id).expect("primary key cannot be built from a u64: use `last_insert_key` instead");

    self.key(key.into_value_tuple().into_iter().collect(), Some(id))
  }

  /// Report `key` as the primary key of the inserted row, as
  /// [`ExecExpectation::last_insert_key`] does, but keeping the number of
  /// affected rows already set.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, Set, prelude::Uuid};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// let id = Uuid::from_u128(42);
  ///
  /// mock.expect_insert::<account::Entity>().rows_affected(1).last_insert_key(id);
  ///
  /// let db = mock.connection().await;
  /// let res = account::Entity::insert(account::ActiveModel { id: Set(id), name: Set("Alice".into()) })
  ///   .exec(&db)
  ///   .await
  ///   .unwrap();
  ///
  /// assert_eq!(res.last_insert_id, id);
  /// # }
  /// ```
  pub fn last_insert_key(self, key: impl Into<KeyOf<E>>) -> Self {
    let values: Vec<Value> = key.into().into_value_tuple().into_iter().collect();

    let id = match values.as_slice() {
      [value] => integer(value).flatten().and_then(|n| u64::try_from(n).ok()),
      _ => None,
    };

    self.key(values, id)
  }

  /// Answer `RETURNING` clauses with a row holding the primary key `values`,
  /// and report `id`, if any, as the last inserted ID. For `cake`, the values
  /// `[5]` give the row `id = 5`.
  fn key(self, values: Vec<Value>, id: Option<u64>) -> Self {
    let row: BTreeMap<String, Value> = E::PrimaryKey::iter().map(|key| key.to_string()).zip(values).collect();

    self.inner.update(|e| {
      if let Some(id) = id {
        e.exec_mut().last_insert_id = id;
      }

      e.response_mut().returning_pk = Some(ProxyRow::new(row));
    });

    self
  }
}

/// Pending untyped expectation, created by [`MockDb::expect_query`](crate::MockDb::expect_query) or
/// [`MockDb::expect_statement`](crate::MockDb::expect_statement).
///
/// For raw SQL, and statements whose results do not map to an entity. Narrow
/// the statements it accepts with the matching methods, then complete it with
/// a result: rows ([`returning_rows`](Self::returning_rows),
/// [`returning_with`](Self::returning_with)), an exec result
/// ([`rows_affected`](Self::rows_affected),
/// [`last_insert_id`](Self::last_insert_id), [`exec_with`](Self::exec_with)),
/// or [`returning_error`](Self::returning_error). Forgetting the result is a
/// compiler warning.
#[must_use = "an expectation needs a result: call `returning_rows(..)`, `returning_with(..)`, `rows_affected(..)`, `last_insert_id(..)`, `exec_with(..)` or `returning_error(..)`"]
pub struct QueryExpectation {
  inner: ExpectationRef,
  backend: DbBackend,
}

impl QueryExpectation {
  pub(crate) fn new(inner: ExpectationRef, backend: DbBackend) -> Self {
    Self { inner, backend }
  }

  matching_methods!();

  /// Complete the expectation by returning these rows: `BTreeMap<String, Value>`s
  /// spelling out every column, or models.
  ///
  /// An empty result needs a type: `Vec::<BTreeMap<String, Value>>::new()`.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use std::collections::BTreeMap;
  ///
  /// use sea_orm::{ConnectionTrait, DbBackend, Statement, Value};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock
  ///   .expect_query()
  ///   .sql_contains("COUNT(*)")
  ///   .returning_rows([BTreeMap::from([("count", Value::BigInt(Some(12)))])]);
  ///
  /// let db = mock.connection().await;
  /// let stmt = Statement::from_string(DbBackend::Postgres, r#"SELECT COUNT(*) AS "count" FROM "cake""#);
  /// let row = db.query_one_raw(stmt).await.unwrap().unwrap();
  ///
  /// assert_eq!(row.try_get::<i64>("", "count").unwrap(), 12);
  /// # }
  /// ```
  pub fn returning_rows<R: IntoMockRow>(self, rows: impl IntoIterator<Item = R>) {
    let rows = rows.into_iter().map(into_proxy_row).collect();
    self.inner.update(|e| {
      let response = e.response_mut();
      response.rows = Some(Rows::Static(rows));
      response.raw_rows = true;
    });
  }

  /// Complete the expectation by computing the returned rows from the incoming
  /// statement. Returning an `Err` fails the statement with it.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use std::collections::BTreeMap;
  ///
  /// use sea_orm::{ConnectionTrait, DbBackend, Statement, Value};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_query().returning_with(|stmt| Ok(vec![BTreeMap::from([("sql", Value::from(stmt.sql.clone()))])]));
  ///
  /// let db = mock.connection().await;
  /// let row = db.query_one_raw(Statement::from_string(DbBackend::Postgres, "SELECT 1")).await.unwrap().unwrap();
  ///
  /// assert_eq!(row.try_get::<String>("", "sql").unwrap(), "SELECT 1");
  /// # }
  /// ```
  pub fn returning_with<R, F>(self, f: F)
  where
    R: IntoMockRow,
    F: Fn(&Statement) -> Result<Vec<R>, DbErr> + Send + Sync + 'static,
  {
    let f = move |stmt: &Statement| Ok(f(stmt)?.into_iter().map(into_proxy_row).collect());
    self.inner.update(|e| e.response_mut().rows = Some(Rows::Fn(Arc::new(f))));
  }

  /// Complete the expectation by reporting `n` affected rows. The returned
  /// [`QueryResponse`] can also report a last inserted ID.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ConnectionTrait, DbBackend};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_statement().sql_contains("TRUNCATE").rows_affected(0);
  ///
  /// let db = mock.connection().await;
  /// db.execute_unprepared(r#"TRUNCATE "cake""#).await.unwrap();
  /// # }
  /// ```
  pub fn rows_affected(self, n: u64) -> QueryResponse {
    QueryResponse(self.inner).rows_affected(n)
  }

  /// Complete the expectation by reporting `id` as the last inserted ID.
  ///
  /// Unlike [`ExecExpectation::last_insert_id`], this knows no entity: it only
  /// sets the exec result, and returns no row to a statement with a `RETURNING`
  /// clause. To answer such a statement, use
  /// [`returning_rows`](Self::returning_rows).
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ConnectionTrait, DbBackend};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::MySql);
  ///
  /// mock.expect_statement().last_insert_id(5).rows_affected(1);
  ///
  /// let db = mock.connection().await;
  /// let res = db.execute_unprepared("INSERT INTO `cake` (`name`) VALUES ('Lemon')").await.unwrap();
  ///
  /// assert_eq!(res.last_insert_id(), 5);
  /// # }
  /// ```
  pub fn last_insert_id(self, id: u64) -> QueryResponse {
    QueryResponse(self.inner).last_insert_id(id)
  }

  /// Complete the expectation by computing the exec result from the incoming
  /// statement. Returning an `Err` fails the statement with it.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ConnectionTrait, DbBackend, ProxyExecResult};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_statement().exec_with(|stmt| Ok(ProxyExecResult::new(0, stmt.sql.len() as u64)));
  ///
  /// let db = mock.connection().await;
  /// let res = db.execute_unprepared("VACUUM").await.unwrap();
  ///
  /// assert_eq!(res.rows_affected(), 6);
  /// # }
  /// ```
  pub fn exec_with<F>(self, f: F)
  where
    F: Fn(&Statement) -> Result<ProxyExecResult, DbErr> + Send + Sync + 'static,
  {
    self.inner.update(|e| e.response_mut().exec = Some(Exec::Fn(Arc::new(f))));
  }
}

/// What [`QueryExpectation::rows_affected`] and
/// [`QueryExpectation::last_insert_id`] return.
///
/// The expectation is already complete. This only lets the rest of its result
/// be set too, in any order: `.last_insert_id(5).rows_affected(2)`.
pub struct QueryResponse(ExpectationRef);

impl QueryResponse {
  /// Report `n` affected rows.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ConnectionTrait, DbBackend};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::MySql);
  ///
  /// mock.expect_statement().last_insert_id(5).rows_affected(2);
  ///
  /// let db = mock.connection().await;
  /// let res = db.execute_unprepared("INSERT INTO `cake` (`name`) VALUES ('A'), ('B')").await.unwrap();
  ///
  /// assert_eq!((res.last_insert_id(), res.rows_affected()), (5, 2));
  /// # }
  /// ```
  pub fn rows_affected(self, n: u64) -> Self {
    self.0.update(|e| e.exec_mut().rows_affected = n);
    self
  }

  /// Report `id` as the last inserted ID.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ConnectionTrait, DbBackend};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::MySql);
  ///
  /// mock.expect_statement().rows_affected(1).last_insert_id(5);
  ///
  /// let db = mock.connection().await;
  /// let res = db.execute_unprepared("INSERT INTO `cake` (`name`) VALUES ('Lemon')").await.unwrap();
  ///
  /// assert_eq!(res.last_insert_id(), 5);
  /// # }
  /// ```
  pub fn last_insert_id(self, id: u64) -> Self {
    self.0.update(|e| e.exec_mut().last_insert_id = id);
    self
  }
}

/// An expected transaction boundary, created by
/// [`MockDb::expect_begin`](crate::MockDb::expect_begin),
/// [`MockDb::expect_commit`](crate::MockDb::expect_commit) or
/// [`MockDb::expect_rollback`](crate::MockDb::expect_rollback).
///
/// Transaction boundaries return nothing, so this needs no result: it only
/// tells how many times the boundary is expected.
pub struct TransactionExpectation {
  inner: ExpectationRef,
}

impl TransactionExpectation {
  pub(crate) fn new(inner: ExpectationRef) -> Self {
    Self { inner }
  }

  /// Expect this boundary exactly `n` times, or up to `n` times when it is also
  /// [`maybe`](Self::maybe). The default is once.
  ///
  /// For nested transactions, each level begins and commits a savepoint: two
  /// levels send `BEGIN`, `BEGIN`, `COMMIT`, `COMMIT`.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, TransactionTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_begin().times(2);
  /// mock.expect_commit().times(2);
  ///
  /// let db = mock.connection().await;
  /// let outer = db.begin().await.unwrap();
  /// outer.begin().await.unwrap().commit().await.unwrap();
  /// outer.commit().await.unwrap();
  /// # }
  /// ```
  pub fn times(self, n: usize) -> Self {
    self.inner.update(|e| {
      e.min = if e.min == 0 { 0 } else { n };
      e.max = n;
    });

    self
  }

  /// Make this boundary optional: it may be met up to its
  /// [`times`](Self::times) (once by default), or not at all.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_begin().maybe();
  /// mock.expect_delete::<cake::Entity>().rows_affected(1);
  /// mock.expect_commit().maybe();
  ///
  /// let db = mock.connection().await;
  /// // Without a transaction: both boundaries are skipped.
  /// cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  /// # }
  /// ```
  pub fn maybe(self) -> Self {
    self.inner.update(|e| e.min = 0);
    self
  }
}
