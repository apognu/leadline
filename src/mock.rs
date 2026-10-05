use std::{
  error::Error,
  fmt,
  sync::{Arc, Mutex, MutexGuard},
};

use sea_orm::{Database, DatabaseConnection, DbBackend, DbErr, EntityTrait, ProxyDatabaseTrait, ProxyExecResult, ProxyRow, Statement};

use crate::{
  builders::{ExecExpectation, QueryExpectation, SelectExpectation, TransactionExpectation},
  classify::{StmtKind, classify},
  expectation::{Expectation, Label, Response, Spec, describe},
  parse::{Parsed, parse},
};

#[derive(Default)]
pub(crate) struct State {
  pub expectations: Vec<Expectation>,
  pub unordered: bool,
  pub strict_transactions: bool,
  pub failures: Vec<String>,
  pub log: Vec<Statement>,
  /// Incremented on every change: a statement received, an expectation added or
  /// changed, a call recorded. Matching compares it before and after running
  /// user code without the lock, to notice changes made in between; `checked`
  /// uses it to know whether `check` already reported the current state.
  revision: usize,
  /// Revision at which `check` was last called.
  checked: Option<usize>,
}

impl State {
  /// The pending expectations, copied out of the state, so that a statement can
  /// be checked against them without holding the lock: matchers may run user
  /// code.
  fn candidates(&self) -> Vec<Candidate> {
    self
      .expectations
      .iter()
      .enumerate()
      .filter(|(_, e)| !e.exhausted())
      .map(|(idx, e)| Candidate {
        idx,
        spec: e.spec.clone(),
        calls: e.calls,
        min: e.min,
        max: e.max,
      })
      .collect()
  }

  /// Record that `stmt` was unexpected, and return the message describing it.
  fn fail(&mut self, kind: StmtKind, stmt: &Statement, reason: &str) -> String {
    let message = format!("unexpected {kind} {}: {reason}", describe(stmt));
    self.failures.push(message.clone());

    message
  }

  fn report(&self) -> Result<(), MockError> {
    let mut problems = self.failures.clone();
    problems.extend(self.expectations.iter().filter_map(|e| {
      if !e.has_result() {
        Some(format!("expectation has no result: {}", e.label()))
      } else if !e.satisfied() {
        Some(format!("expectation not met: {}", e.label()))
      } else {
        None
      }
    }));

    if problems.is_empty() { Ok(()) } else { Err(MockError(problems)) }
  }
}

/// A scripted mock database: the statements it expects, what each of them
/// returns, and everything it received.
///
/// Declare expectations with the `expect_*` methods, then hand
/// [`connection`](Self::connection) to the code under test. `MockDb` is a cheap
/// handle: clones share the same expectations, so it can be passed around
/// freely.
///
/// # Matching
///
/// Statements must arrive in the order their expectations were declared,
/// unless [`unordered`](Self::unordered) is called. Each statement is matched
/// against the next pending expectation: its kind (`SELECT`, `INSERT`, ...),
/// its entity's table for typed expectations, its SQL matchers and its
/// arguments must all agree.
///
/// # Failures
///
/// A statement no expectation accepts makes the call panic with what was
/// expected instead, failing the test.
///
/// There is no need to verify expectations at the end of a test: dropping the
/// last clone of the mock panics with every problem found, unless already
/// reported by [`check`](Self::check) or [`verify`](Self::verify). That covers
/// unmet expectations, expectations without a result, and unexpected statements
/// whose panic was swallowed, e.g. by a spawned task. Nothing is reported if the
/// test is already panicking.
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
/// mock.expect_delete::<cake::Entity>().with_args((1,)).rows_affected(1);
///
/// let db = mock.connection().await;
/// cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
/// # }
/// ```
#[derive(Clone)]
pub struct MockDb {
  pub(crate) backend: DbBackend,
  pub(crate) state: Arc<Mutex<State>>,
  _drop_check: Arc<DropCheck>,
}

impl MockDb {
  /// Create a mock of a `backend` database, with no expectations.
  ///
  /// The backend decides the SQL dialect SeaORM generates, and what it sends for
  /// some operations: Postgres and SQLite read written rows back with
  /// `RETURNING`, while MySQL does not. Use the backend the code under test runs
  /// against.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::DbBackend;
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::MySql);
  /// let db = mock.connection().await;
  ///
  /// assert_eq!(db.get_database_backend(), DbBackend::MySql);
  /// # }
  /// ```
  pub fn new(backend: DbBackend) -> Self {
    let state = Arc::<Mutex<State>>::default();

    Self {
      backend,
      _drop_check: Arc::new(DropCheck { state: state.clone() }),
      state,
    }
  }

  /// Let expectations be met in any order.
  ///
  /// By default, statements must arrive in the order their expectations were
  /// declared, which also tests the order the code sends them. When that order
  /// does not matter, or is not deterministic (e.g. concurrent tasks), each
  /// statement is matched against every pending expectation instead.
  ///
  /// Returns the mock, so that it can be set up in one expression:
  /// `let mock = MockDb::new(backend).unordered();`.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres).unordered();
  ///
  /// mock.expect_delete::<cake::Entity>().rows_affected(1);
  /// mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);
  ///
  /// let db = mock.connection().await;
  /// cake::Entity::find().all(&db).await.unwrap();
  /// cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  /// # }
  /// ```
  pub fn unordered(&self) -> Self {
    self.lock().unordered = true;
    self.clone()
  }

  /// Check every `BEGIN`, `COMMIT` and `ROLLBACK` against expectations.
  ///
  /// By default, transactions are only checked once at least one transaction
  /// boundary is expected, so that tests not interested in transactions do not
  /// have to script them. Until then, a transaction boundary is answered by an
  /// [`expect_statement`](Self::expect_statement) expectation matching it, if
  /// any, and ignored otherwise. After `strict_transactions()`, every
  /// transaction boundary is checked, even when none is expected: for example,
  /// to make sure that some code does not open a transaction.
  ///
  /// Returns the mock, so that it can be set up in one expression.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres).strict_transactions();
  ///
  /// mock.expect_delete::<cake::Entity>().rows_affected(1);
  ///
  /// let db = mock.connection().await;
  /// // Without a transaction: an unexpected BEGIN would fail the test.
  /// cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  /// # }
  /// ```
  pub fn strict_transactions(&self) -> Self {
    self.lock().strict_transactions = true;
    self.clone()
  }

  /// Create a [`DatabaseConnection`] backed by this mock, to hand to the code
  /// under test.
  ///
  /// It is a real `DatabaseConnection`, using SeaORM's proxy driver, so code
  /// taking `&DatabaseConnection`, `impl ConnectionTrait` or
  /// `impl TransactionTrait` runs unchanged, transactions included. Every
  /// connection of a mock shares its expectations.
  ///
  /// # Panics
  ///
  /// If SeaORM cannot create the proxy connection, which does not happen with
  /// the supported backends.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DatabaseConnection, DbBackend, DbErr, EntityTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// async fn count_cakes(db: &DatabaseConnection) -> Result<usize, DbErr> {
  ///   Ok(cake::Entity::find().all(db).await?.len())
  /// }
  ///
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);
  ///
  /// let db = mock.connection().await;
  /// assert_eq!(count_cakes(&db).await.unwrap(), 0);
  /// # }
  /// ```
  pub async fn connection(&self) -> DatabaseConnection {
    let handler: Box<dyn ProxyDatabaseTrait> = Box::new(Handler {
      state: self.state.clone(),
      backend: self.backend,
    });

    Database::connect_proxy(self.backend, Arc::new(handler)).await.expect("could not create proxy connection")
  }

  /// Expect a `SELECT` on the entity `E`, returning `E`'s models.
  ///
  /// The statement must be on `E`'s table: `SELECT … FROM "bakery" JOIN "cake"
  /// …` is on `bakery`, not on `cake`. When the SQL does not parse, any mention
  /// of the table is enough. [`SelectExpectation`] describes how to narrow the
  /// expectation down and set its result.
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
  ///   .matching(cake::Entity::find())
  ///   .returning([cake::Model { id: 1, name: "Chocolate".into(), bakery_id: None }]);
  ///
  /// let db = mock.connection().await;
  /// assert_eq!(cake::Entity::find().all(&db).await.unwrap().len(), 1);
  /// # }
  /// ```
  pub fn expect_select<E: EntityTrait>(&self) -> SelectExpectation<E> {
    SelectExpectation::new(self.push_entity::<E>(StmtKind::Select), self.backend)
  }

  /// Expect an `INSERT` into the entity `E`.
  ///
  /// The statement must be on `E`'s table, as for
  /// [`expect_select`](Self::expect_select). [`ExecExpectation`] describes how
  /// to narrow the expectation down and set its result.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, Set};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_insert::<cake::Entity>().with_args(("Lemon",)).last_insert_id(1);
  ///
  /// let db = mock.connection().await;
  /// cake::Entity::insert(cake::ActiveModel { name: Set("Lemon".into()), ..Default::default() })
  ///   .exec(&db)
  ///   .await
  ///   .unwrap();
  /// # }
  /// ```
  pub fn expect_insert<E: EntityTrait>(&self) -> ExecExpectation<E> {
    ExecExpectation::new(self.push_entity::<E>(StmtKind::Insert), self.backend)
  }

  /// Expect an `UPDATE` of the entity `E`.
  ///
  /// The statement must be on `E`'s table, as for
  /// [`expect_select`](Self::expect_select). [`ExecExpectation`] describes how
  /// to narrow the expectation down and set its result.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{ColumnTrait, DbBackend, EntityTrait, QueryFilter, sea_query::Expr};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_update::<cake::Entity>().with_args(("Lemon", 1)).rows_affected(1);
  ///
  /// let db = mock.connection().await;
  /// cake::Entity::update_many()
  ///   .col_expr(cake::Column::Name, Expr::value("Lemon"))
  ///   .filter(cake::Column::Id.eq(1))
  ///   .exec(&db)
  ///   .await
  ///   .unwrap();
  /// # }
  /// ```
  pub fn expect_update<E: EntityTrait>(&self) -> ExecExpectation<E> {
    ExecExpectation::new(self.push_entity::<E>(StmtKind::Update), self.backend)
  }

  /// Expect a `DELETE` from the entity `E`.
  ///
  /// The statement must be on `E`'s table, as for
  /// [`expect_select`](Self::expect_select). [`ExecExpectation`] describes how
  /// to narrow the expectation down and set its result.
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
  /// mock.expect_delete::<cake::Entity>().matching(cake::Entity::delete_by_id(1)).rows_affected(1);
  ///
  /// let db = mock.connection().await;
  /// cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  /// # }
  /// ```
  pub fn expect_delete<E: EntityTrait>(&self) -> ExecExpectation<E> {
    ExecExpectation::new(self.push_entity::<E>(StmtKind::Delete), self.backend)
  }

  /// Expect a `SELECT` on any table, with untyped results.
  ///
  /// For queries whose rows do not map to an entity: aggregates, raw SQL,
  /// custom selections. See [`QueryExpectation`].
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
  /// mock.expect_query().returning_rows([BTreeMap::from([("now", Value::from("2026-01-01"))])]);
  ///
  /// let db = mock.connection().await;
  /// let row = db.query_one_raw(Statement::from_string(DbBackend::Postgres, "SELECT NOW()::text AS now")).await.unwrap();
  ///
  /// assert!(row.is_some());
  /// # }
  /// ```
  pub fn expect_query(&self) -> QueryExpectation {
    QueryExpectation::new(self.push(Expectation::new(Some(StmtKind::Select))), self.backend)
  }

  /// Expect a statement of any kind, with untyped results.
  ///
  /// For statements no other expectation fits: DDL, `TRUNCATE`, vendor-specific
  /// commands, or raw writes. See [`QueryExpectation`].
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
  /// mock.expect_statement().sql(r#"CREATE INDEX "idx" ON "cake" ("name")"#).rows_affected(0);
  ///
  /// let db = mock.connection().await;
  /// db.execute_unprepared(r#"CREATE INDEX "idx" ON "cake" ("name")"#).await.unwrap();
  /// # }
  /// ```
  pub fn expect_statement(&self) -> QueryExpectation {
    QueryExpectation::new(self.push(Expectation::new(None)), self.backend)
  }

  /// Expect a transaction (or savepoint) to begin.
  ///
  /// Expecting any `BEGIN`, `COMMIT` or `ROLLBACK` makes all of them checked;
  /// see [`strict_transactions`](Self::strict_transactions). Nested transactions
  /// begin and commit too: see [`TransactionExpectation::times`].
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait, TransactionTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_begin();
  /// mock.expect_delete::<cake::Entity>().rows_affected(1);
  /// mock.expect_commit();
  ///
  /// let db = mock.connection().await;
  /// let tx = db.begin().await.unwrap();
  /// cake::Entity::delete_by_id(1).exec(&tx).await.unwrap();
  /// tx.commit().await.unwrap();
  /// # }
  /// ```
  pub fn expect_begin(&self) -> TransactionExpectation {
    TransactionExpectation::new(self.push(Expectation::new(Some(StmtKind::Begin))))
  }

  /// Expect a transaction (or savepoint) to commit. See
  /// [`expect_begin`](Self::expect_begin).
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, DbErr, TransactionTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_begin();
  /// mock.expect_commit();
  ///
  /// let db = mock.connection().await;
  /// db.transaction::<_, _, DbErr>(|_tx| Box::pin(async { Ok(()) })).await.unwrap();
  /// # }
  /// ```
  pub fn expect_commit(&self) -> TransactionExpectation {
    TransactionExpectation::new(self.push(Expectation::new(Some(StmtKind::Commit))))
  }

  /// Expect a transaction (or savepoint) to roll back, explicitly or by being
  /// dropped. See [`expect_begin`](Self::expect_begin).
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, DbErr, EntityTrait, TransactionTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  ///
  /// mock.expect_begin();
  /// mock.expect_delete::<cake::Entity>().returning_error(DbErr::Custom("boom".into()));
  /// mock.expect_rollback();
  ///
  /// let db = mock.connection().await;
  /// let res = db
  ///   .transaction::<_, (), DbErr>(|tx| {
  ///     Box::pin(async move {
  ///       cake::Entity::delete_by_id(1).exec(tx).await?;
  ///       Ok(())
  ///     })
  ///   })
  ///   .await;
  ///
  /// assert!(res.is_err());
  /// # }
  /// ```
  pub fn expect_rollback(&self) -> TransactionExpectation {
    TransactionExpectation::new(self.push(Expectation::new(Some(StmtKind::Rollback))))
  }

  /// Every statement received so far, in order, expected or not.
  ///
  /// Transactions are logged as `BEGIN`, `COMMIT` and `ROLLBACK` statements.
  /// Useful to inspect what the code under test sent, e.g. to debug a failing
  /// expectation or assert on details no matcher covers.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);
  ///
  /// let db = mock.connection().await;
  /// cake::Entity::find().all(&db).await.unwrap();
  ///
  /// assert!(mock.statements()[0].sql.starts_with(r#"SELECT "cake"."id""#));
  /// # }
  /// ```
  pub fn statements(&self) -> Vec<Statement> {
    self.lock().log.clone()
  }

  /// Report every problem found so far: unmet expectations, expectations
  /// without a result, and unexpected statements.
  ///
  /// Use it to assert on failures themselves. Problems it reports are not
  /// reported again when the mock is dropped, unless more statements or
  /// expectations come after the check.
  ///
  /// # Errors
  ///
  /// A [`MockError`] listing the problems, if any.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::DbBackend;
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// mock.expect_delete::<cake::Entity>().rows_affected(1);
  ///
  /// let err = mock.check().unwrap_err();
  /// assert!(err.problems()[0].starts_with("expectation not met: DELETE on `cake`"));
  /// # }
  /// ```
  pub fn check(&self) -> Result<(), MockError> {
    let mut state = self.lock();
    state.checked = Some(state.revision);

    state.report()
  }

  /// Fail the test if any problem was found so far, like [`check`](Self::check)
  /// but panicking with the report, at the caller's location.
  ///
  /// Not needed at the end of a test, where dropping the mock checks the same.
  /// Use it to check progress mid-test, before scripting the next phase, or when
  /// the mock outlives the test.
  ///
  /// # Panics
  ///
  /// If [`check`](Self::check) finds a problem.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::{DbBackend, EntityTrait};
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// mock.expect_delete::<cake::Entity>().rows_affected(1);
  ///
  /// let db = mock.connection().await;
  /// cake::Entity::delete_by_id(1).exec(&db).await.unwrap();
  /// mock.verify();
  ///
  /// mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);
  /// cake::Entity::find().all(&db).await.unwrap();
  /// # }
  /// ```
  #[track_caller]
  pub fn verify(&self) {
    if let Err(err) = self.check() {
      panic!("{err}");
    }
  }

  fn push_entity<E: EntityTrait>(&self, kind: StmtKind) -> ExpectationRef {
    let mut expectation = Expectation::new(Some(kind));
    expectation.spec_mut().table = Some(E::default().table_name().to_string());

    self.push(expectation)
  }

  fn push(&self, expectation: Expectation) -> ExpectationRef {
    let mut state = self.lock();
    state.revision += 1;
    state.expectations.push(expectation);

    ExpectationRef {
      state: self.state.clone(),
      index: state.expectations.len() - 1,
    }
  }

  fn lock(&self) -> MutexGuard<'_, State> {
    lock(&self.state)
  }
}

/// Handle to an expectation stored in the mock, used by the builders.
pub(crate) struct ExpectationRef {
  state: Arc<Mutex<State>>,
  index: usize,
}

impl ExpectationRef {
  pub fn update(&self, f: impl FnOnce(&mut Expectation)) {
    let mut state = lock(&self.state);
    state.revision += 1;

    f(&mut state.expectations[self.index]);
  }
}

/// Owned by every clone of a [`MockDb`] (but not by its connections), to
/// report problems when the last one is dropped.
struct DropCheck {
  state: Arc<Mutex<State>>,
}

impl Drop for DropCheck {
  fn drop(&mut self) {
    // Panicking while unwinding would abort the process.
    if std::thread::panicking() {
      return;
    }

    let state = lock(&self.state);

    if state.checked == Some(state.revision) {
      return;
    }

    if let Err(err) = state.report() {
      drop(state);
      panic!("{err}");
    }
  }
}

/// The problems found by [`MockDb::check`], one message per problem.
///
/// Its `Display` lists them all: it is the report [`MockDb::verify`] panics
/// with, as does the check run when the mock is dropped.
///
/// ```
/// # include!("../doctests/entities.rs");
/// use sea_orm::DbBackend;
/// use leadline::MockDb;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let mock = MockDb::new(DbBackend::Postgres);
/// mock.expect_delete::<cake::Entity>().rows_affected(1);
///
/// let err = mock.check().unwrap_err();
/// assert_eq!(err.problems().len(), 1);
/// assert!(err.to_string().starts_with("leadline: 1 problem(s):"));
/// # }
/// ```
#[derive(Debug)]
pub struct MockError(Vec<String>);

impl MockError {
  /// The message of each problem, in the order they were found: unexpected
  /// statements first, then expectations without a result or not met.
  ///
  /// Useful to assert on a specific problem, where `Display` gives the whole
  /// report.
  ///
  /// ```
  /// # include!("../doctests/entities.rs");
  /// use sea_orm::DbBackend;
  /// use leadline::MockDb;
  ///
  /// # #[tokio::main(flavor = "current_thread")]
  /// # async fn main() {
  /// let mock = MockDb::new(DbBackend::Postgres);
  /// mock.expect_delete::<cake::Entity>().rows_affected(1);
  ///
  /// let err = mock.check().unwrap_err();
  /// assert_eq!(err.problems(), ["expectation not met: DELETE on `cake` with any SQL"]);
  /// # }
  /// ```
  pub fn problems(&self) -> &[String] {
    &self.0
  }
}

impl fmt::Display for MockError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    writeln!(f, "leadline: {} problem(s):", self.0.len())?;

    for problem in &self.0 {
      writeln!(f, "  - {problem}")?;
    }

    Ok(())
  }
}

impl Error for MockError {}

#[derive(Debug)]
struct Handler {
  state: Arc<Mutex<State>>,
  /// The mock's backend, given to the statements standing for transaction
  /// boundaries (`BEGIN`, `COMMIT` and `ROLLBACK`).
  backend: DbBackend,
}

impl fmt::Debug for State {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("State").field("expectations", &self.expectations.len()).finish_non_exhaustive()
  }
}

impl Handler {
  /// Find the expectation answering `stmt`, and compute its response with
  /// `respond`; `unscripted` answers ignored transaction boundaries. Fails the
  /// test if no expectation answers.
  ///
  /// `reads_rows` tells whether the statement was sent through
  /// `ConnectionTrait::query_*`, which reads rows back. A write sent there,
  /// such as `INSERT … RETURNING`, needs an expectation with rows: one with
  /// only an exec result (`rows_affected`) fails the test with an explanation,
  /// rather than silently returning no rows.
  fn respond<T>(&self, stmt: Statement, reads_rows: bool, respond: impl FnOnce(&Selected, &Statement, Option<&Parsed>) -> Result<T, DbErr>, unscripted: T) -> Result<T, DbErr> {
    // Parsing gives the kind of data-modifying CTEs (`WITH … DELETE`) too;
    // the leading keyword is the fallback for SQL that does not parse.
    let parsed = parse(&stmt);
    let kind = parsed.as_ref().map_or_else(|| classify(&stmt.sql), |parsed| parsed.kind);

    match self.select(kind, &stmt, parsed.as_ref()) {
      Ok(Some(selected)) if reads_rows && matches!(kind, StmtKind::Insert | StmtKind::Update | StmtKind::Delete) && !selected.response.has_rows() => {
        let fix = match (selected.table.is_some(), kind) {
          (true, StmtKind::Insert) => "`.returning(..)`, `.last_insert_id(..)` or `.last_insert_key(..)`",
          (true, _) => "`.returning(..)` (`.returning::<Model>([])` for none)",
          (false, _) => "`.returning_rows(..)` (an empty list for none)",
        };

        let reason = format!("it reads the written rows back (`RETURNING` on this backend), but its expectation has no rows to return: complete it with {fix}");
        let message = lock(&self.state).fail(kind, &stmt, &reason);

        unexpected(message)
      }
      // The response may call user code: the lock is not held anymore.
      Ok(Some(selected)) => respond(&selected, &stmt, parsed.as_ref()),
      Ok(None) => Ok(unscripted),
      Err(message) => unexpected(message),
    }
  }

  fn transaction(&self, kind: StmtKind) {
    let stmt = Statement::from_string(self.backend, kind.to_string());

    if let Err(message) = self.select(kind, &stmt, None) {
      let _ = unexpected::<()>(message);
    }
  }

  /// Find the expectation answering `stmt`, record the call, and return its
  /// response. Returns `None` for an ignored transaction boundary. On failure,
  /// records and returns why no expectation matched.
  ///
  /// Matchers may run user code (`sql_fn`, `Arg::matching`), which may call the
  /// mock and take its lock. So the statement is matched against a copy of the
  /// pending expectations, without the lock. If the expectations changed in the
  /// meantime, the statement is matched again against the new ones.
  fn select(&self, kind: StmtKind, stmt: &Statement, parsed: Option<&Parsed>) -> Result<Option<Selected>, String> {
    // Until transactions are checked, a transaction statement is answered by
    // an expectation matching it, and ignored otherwise.
    let unchecked_transaction = {
      let mut state = lock(&self.state);
      state.revision += 1;
      state.log.push(stmt.clone());

      kind.is_transaction() && !state.strict_transactions && !state.expectations.iter().any(|e| e.spec.is_transaction())
    };

    loop {
      let (candidates, total, unordered, revision) = {
        let state = lock(&self.state);
        (state.candidates(), state.expectations.len(), state.unordered, state.revision)
      };

      let choice = if unordered {
        choose_unordered(&candidates, total, kind, stmt, parsed)
      } else {
        choose_ordered(&candidates, total, kind, stmt, parsed)
      };

      let mut state = lock(&self.state);

      if state.revision != revision {
        continue;
      }

      let (idx, skipped) = match choice {
        Ok(choice) => choice,
        Err(_) if unchecked_transaction => return Ok(None),
        Err(reason) => return Err(state.fail(kind, stmt, &reason)),
      };

      if !state.expectations[idx].has_result() {
        let reason = format!("it matches {}, which has no result", state.expectations[idx].label());

        return Err(state.fail(kind, stmt, &reason));
      }

      for idx in skipped {
        state.expectations[idx].close();
      }

      state.revision += 1;

      let expectation = &mut state.expectations[idx];
      expectation.calls += 1;

      return Ok(Some(Selected {
        response: expectation.response.clone(),
        table: expectation.spec.table.clone(),
      }));
    }
  }
}

/// The expectation answering a statement: its response, and its entity's
/// table, if any.
struct Selected {
  response: Arc<Response>,
  table: Option<String>,
}

/// A pending expectation, as it stood when a statement arrived.
struct Candidate {
  idx: usize,
  spec: Arc<Spec>,
  calls: usize,
  min: usize,
  max: usize,
}

impl Candidate {
  fn satisfied(&self) -> bool {
    self.calls >= self.min
  }

  fn label(&self) -> Label<'_> {
    Label {
      spec: &self.spec,
      calls: self.calls,
      min: self.min,
      max: self.max,
    }
  }
}

/// The first pending expectation matching `stmt`, in any order.
fn choose_unordered(candidates: &[Candidate], total: usize, kind: StmtKind, stmt: &Statement, parsed: Option<&Parsed>) -> Result<(usize, Vec<usize>), String> {
  let mut mismatches = Vec::new();

  for candidate in candidates {
    match candidate.spec.check(kind, stmt, parsed) {
      Ok(()) => return Ok((candidate.idx, Vec::new())),
      Err(reason) => mismatches.push(format!("{}: {reason}", candidate.label())),
    }
  }

  Err(no_match(total, &mismatches, "pending"))
}

/// Find the expectation answering `stmt` in an ordered mock: the first pending
/// expectation must match.
///
/// An expectation already satisfied (optional, or `times(n)` with enough calls)
/// can be skipped when it does not match, and the next one is tried. With an
/// optional `SELECT` expected before a `DELETE`, a `DELETE` skips the `SELECT`,
/// but an `UPDATE` fails, as the `DELETE` does not match it either. Returns the
/// matching expectation, and the skipped ones, which the caller closes.
fn choose_ordered(candidates: &[Candidate], total: usize, kind: StmtKind, stmt: &Statement, parsed: Option<&Parsed>) -> Result<(usize, Vec<usize>), String> {
  let mut skipped = Vec::new();
  let mut mismatches = Vec::new();

  for candidate in candidates {
    match candidate.spec.check(kind, stmt, parsed) {
      Ok(()) => return Ok((candidate.idx, skipped)),

      Err(reason) if candidate.satisfied() => {
        skipped.push(candidate.idx);
        mismatches.push(format!("{}: {reason}", candidate.label()));
      }

      Err(reason) => return Err(format!("next expectation is {}, but {reason}", candidate.label())),
    }
  }

  Err(no_match(total, &mismatches, "remaining optional"))
}

/// Explain why no expectation answered a statement, given the number of
/// expectations and why each candidate did not match.
fn no_match(total: usize, mismatches: &[String], candidates: &str) -> String {
  if total == 0 {
    return "no expectation was set".to_string();
  }

  if mismatches.is_empty() {
    return "every expectation was already consumed".to_string();
  }

  let mut message = format!("none of the {candidates} expectations matches it:");

  for mismatch in mismatches {
    message.push_str(&format!("\n      - {mismatch}"));
  }

  message
}

/// Fail the test with `message`, by panicking.
///
/// When the thread is already panicking, as when a transaction is rolled back
/// on drop during a failing test, a second panic would abort the process: this
/// returns an error instead. The failure is recorded for the report anyway.
fn unexpected<T>(message: String) -> Result<T, DbErr> {
  if !std::thread::panicking() {
    panic!("leadline: {message}");
  }

  Err(DbErr::Custom(format!("leadline: {message}")))
}

#[async_trait::async_trait]
impl ProxyDatabaseTrait for Handler {
  async fn query(&self, stmt: Statement) -> Result<Vec<ProxyRow>, DbErr> {
    self.respond(stmt, true, |selected, stmt, parsed| selected.response.query(selected.table.as_deref(), stmt, parsed), Vec::new())
  }

  async fn execute(&self, stmt: Statement) -> Result<ProxyExecResult, DbErr> {
    self.respond(stmt, false, |selected, stmt, _| selected.response.exec(stmt), ProxyExecResult::default())
  }

  async fn begin(&self) {
    self.transaction(StmtKind::Begin);
  }

  async fn commit(&self) {
    self.transaction(StmtKind::Commit);
  }

  async fn rollback(&self) {
    self.transaction(StmtKind::Rollback);
  }

  fn start_rollback(&self) {
    self.transaction(StmtKind::Rollback);
  }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
  // A panicking closure must not hide every later report.
  state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
