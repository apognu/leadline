use std::{
  error::Error,
  fmt,
  panic::Location,
  sync::{Arc, Mutex, MutexGuard},
};

use sea_orm::{Database, DatabaseConnection, DbBackend, DbErr, EntityTrait, ProxyDatabaseTrait, ProxyExecResult, ProxyRow, Statement};

use crate::{
  builders::{ExecExpectation, QueryExpectation, SelectExpectation, TransactionExpectation},
  classify::{StmtKind, classify},
  expectation::{Expectation, Response},
  parse::{Parsed, parse},
  problem::{Mismatch, Problem, Reason},
  render,
};

#[derive(Default)]
pub(crate) struct State {
  pub expectations: Vec<Expectation>,
  pub unordered: bool,
  pub strict_transactions: bool,
  pub failures: Vec<Problem>,
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
  /// Copy the pending expectations (those that can still be called) out of
  /// the state, each with its index in `expectations`.
  ///
  /// A statement is checked against these copies without holding the lock,
  /// because checking may run the user's closures (`sql_fn`, `Arg::matching`),
  /// which may use the mock too. The copies are cheap: what an expectation
  /// matches and answers is behind `Arc`s.
  fn candidates(&self) -> Vec<(usize, Expectation)> {
    self.expectations.iter().enumerate().filter(|(_, e)| !e.exhausted()).map(|(idx, e)| (idx, e.clone())).collect()
  }

  /// Record that no expectation answered `stmt`, for the report made when the
  /// mock is checked, and return the problem, to panic with it now.
  fn fail(&mut self, kind: StmtKind, stmt: &Statement, reason: Reason) -> Box<Problem> {
    let problem = Problem::Unexpected { kind, stmt: stmt.clone(), reason };

    self.failures.push(problem.clone());

    Box::new(problem)
  }

  /// List every problem found so far: the unexpected statements, in the order
  /// they arrived, then the expectations without a result or not called
  /// enough, in the order they were declared.
  fn report(&self) -> Result<(), MockError> {
    let mut problems = self.failures.clone();
    problems.extend(self.expectations.iter().filter_map(|e| {
      if !e.has_result() {
        Some(Problem::NoResult(e.declared()))
      } else if !e.satisfied() {
        Some(Problem::Unmet(e.declared()))
      } else {
        None
      }
    }));

    if problems.is_empty() { Ok(()) } else { Err(MockError::new(problems)) }
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
  #[track_caller]
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
  #[track_caller]
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
  #[track_caller]
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
  #[track_caller]
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
  #[track_caller]
  pub fn expect_query(&self) -> QueryExpectation {
    QueryExpectation::new(self.push(Expectation::new(Some(StmtKind::Select), Location::caller())), self.backend)
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
  #[track_caller]
  pub fn expect_statement(&self) -> QueryExpectation {
    QueryExpectation::new(self.push(Expectation::new(None, Location::caller())), self.backend)
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
  #[track_caller]
  pub fn expect_begin(&self) -> TransactionExpectation {
    TransactionExpectation::new(self.push(Expectation::new(Some(StmtKind::Begin), Location::caller())))
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
  #[track_caller]
  pub fn expect_commit(&self) -> TransactionExpectation {
    TransactionExpectation::new(self.push(Expectation::new(Some(StmtKind::Commit), Location::caller())))
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
  #[track_caller]
  pub fn expect_rollback(&self) -> TransactionExpectation {
    TransactionExpectation::new(self.push(Expectation::new(Some(StmtKind::Rollback), Location::caller())))
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
      panic!("{}", err.report(render::colors()));
    }
  }

  #[track_caller]
  fn push_entity<E: EntityTrait>(&self, kind: StmtKind) -> ExpectationRef {
    let mut expectation = Expectation::new(Some(kind), Location::caller());
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
  pub(crate) fn update(&self, f: impl FnOnce(&mut Expectation)) {
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
      panic!("{}", err.report(render::colors()));
    }
  }
}

/// The problems found by [`MockDb::check`]: unexpected statements, and
/// expectations without a result or not met.
///
/// There are two ways to read them:
///
/// - [`problems`](Self::problems) gives one plain-text message for each
///   problem, with all its details. This is what a test checking a failure
///   should assert on.
/// - `Display` gives the report that [`MockDb::verify`], and the check run
///   when the mock is dropped, panic with: a line counting the problems, a
///   line summing up each one, then a diagnostic for each one, in the style
///   of the Rust compiler's errors. `Display` never uses colors; the panics
///   do, when stderr and stdout are terminals.
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
/// assert!(err.to_string().starts_with("leadline: 1 problem:\n  - expectation not met: DELETE on `cake`"));
/// # }
/// ```
pub struct MockError {
  pub(crate) problems: Vec<Problem>,
  messages: Vec<String>,
}

impl MockError {
  /// The message of each problem, in plain text: the unexpected statements
  /// first, in the order they arrived, then the expectations without a result
  /// or not met, in the order they were declared.
  ///
  /// Each message gives the full details on one line (or more, when several
  /// expectations rejected a statement), without colors, which makes it the
  /// thing to assert on in a test checking a failure:
  ///
  /// ```text
  /// unexpected SELECT `SELECT "cake"."id" FROM "cake"` with []: the next expectation does not match it (DELETE on `cake` with any SQL): expected a DELETE statement, got SELECT
  /// expectation not met: DELETE on `cake` with any SQL
  /// ```
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
    &self.messages
  }

  fn new(problems: Vec<Problem>) -> Self {
    let messages = problems.iter().map(Problem::to_string).collect();

    Self { problems, messages }
  }

  /// Write the report on the problems. `Display` gives it without colors. The
  /// panics of `verify` and of the drop check give it in color when
  /// `render::colors` allows it:
  ///
  /// ```text
  /// leadline: 2 problems:
  ///   - unexpected SELECT: no expectation was set
  ///   - expectation not met: DELETE on `cake` with any SQL
  ///
  /// error: unexpected SELECT
  /// … (a diagnostic for each problem, see `render::render`)
  /// ```
  ///
  /// The first lines stay plain even with `colors`, so that
  /// `#[should_panic(expected = …)]` and searches find them.
  pub(crate) fn report(&self, colors: bool) -> String {
    let count = self.problems.len();
    let plural = if count == 1 { "" } else { "s" };

    // The summaries stay plain, for `should_panic` and searches.
    let mut report = format!("leadline: {count} problem{plural}:\n");

    for problem in &self.problems {
      report.push_str(&format!("  - {}\n", problem.headline()));
    }

    report.push('\n');
    report.push_str(&render::render(&self.problems, colors));

    report
  }
}

impl fmt::Display for MockError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.report(false))
  }
}

impl fmt::Debug for MockError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_tuple("MockError").field(&self.messages).finish()
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
  /// Find the expectation answering `stmt`, and compute the response with
  /// `answer`, which gets that expectation. Fails the test if no expectation
  /// answers `stmt`.
  ///
  /// `unscripted` is the response to a `BEGIN`, `COMMIT` or `ROLLBACK` sent
  /// as SQL (as with `execute_unprepared("BEGIN")`) while transactions are not
  /// checked (see `MockDb::strict_transactions`): no expectation answers it,
  /// and it gets this empty response instead of failing the test.
  ///
  /// `reads_rows` tells whether the statement was sent through
  /// `ConnectionTrait::query_*`, which reads rows back. A write sent there,
  /// such as `INSERT … RETURNING`, needs an expectation with rows: one with
  /// only an exec result (`rows_affected`) fails the test with an explanation,
  /// rather than silently returning no rows.
  fn respond<T>(&self, stmt: Statement, reads_rows: bool, answer: impl FnOnce(&Selected, &Statement, Option<&Parsed>) -> Result<T, DbErr>, unscripted: T) -> Result<T, DbErr> {
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

        let problem = lock(&self.state).fail(kind, &stmt, Reason::MissingRows { fix });

        Err(unexpected(problem))
      }
      // The response may call user code: the lock is not held anymore.
      Ok(Some(selected)) => answer(&selected, &stmt, parsed.as_ref()),
      Ok(None) => Ok(unscripted),
      Err(problem) => Err(unexpected(problem)),
    }
  }

  /// Handle a transaction boundary: `BEGIN`, `COMMIT` or `ROLLBACK`. It is
  /// matched like a statement whose SQL is its keyword.
  fn transaction(&self, kind: StmtKind) {
    let stmt = Statement::from_string(self.backend, kind.to_string());

    // Transaction boundaries cannot return an error: the problem is recorded
    // for the report anyway.
    if let Err(problem) = self.select(kind, &stmt, None) {
      unexpected(problem);
    }
  }

  /// Log `stmt`, find the expectation answering it, record the call, and
  /// return that expectation's response.
  ///
  /// Returns `Ok(None)` for a transaction boundary that is not checked (see
  /// `MockDb::strict_transactions`) and that no expectation matches. Fails
  /// when no expectation matches `stmt`, or when the one that matches has no
  /// result: the problem is then recorded for the report, and returned.
  ///
  /// Matchers may run user code (`sql_fn`, `Arg::matching`), which may call the
  /// mock and take its lock. So the statement is matched against a copy of the
  /// pending expectations, without the lock. If the expectations changed in the
  /// meantime, the statement is matched again against the new ones.
  fn select(&self, kind: StmtKind, stmt: &Statement, parsed: Option<&Parsed>) -> Result<Option<Selected>, Box<Problem>> {
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

      let choice = choose(&candidates, total, unordered, kind, stmt, parsed);

      let mut state = lock(&self.state);

      if state.revision != revision {
        continue;
      }

      let (idx, skipped) = match choice {
        Ok(choice) => choice,
        Err(_) if unchecked_transaction => return Ok(None),
        Err(reason) => return Err(state.fail(kind, stmt, reason)),
      };

      if !state.expectations[idx].has_result() {
        let reason = Reason::MatchedWithoutResult(state.expectations[idx].declared());

        return Err(state.fail(kind, stmt, reason));
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

/// Find the expectation answering `stmt` among the pending ones (`candidates`,
/// with their index), and return its index, with the indexes of the
/// expectations it skipped, which the caller closes. `total` is the number of
/// expectations, pending or not.
///
/// An unordered mock takes the first pending expectation that matches, and
/// skips none.
///
/// An ordered mock requires the next pending expectation to match. It may
/// skip that one only if it is already satisfied (optional, or `times(n)`
/// with enough calls), and try the one after it, and so on. With an optional
/// `SELECT` expected before a `DELETE`:
///
/// - a `DELETE` skips the `SELECT` and matches the `DELETE`: the `SELECT` is
///   closed, and will not match later statements;
/// - an `UPDATE` fails: it matches neither of them.
fn choose(candidates: &[(usize, Expectation)], total: usize, unordered: bool, kind: StmtKind, stmt: &Statement, parsed: Option<&Parsed>) -> Result<(usize, Vec<usize>), Reason> {
  let mut skipped = Vec::new();
  let mut rejections = Vec::new();

  for (idx, candidate) in candidates {
    match candidate.spec.check(kind, stmt, parsed) {
      Ok(()) => return Ok((*idx, skipped)),

      Err(mismatch) if unordered || candidate.satisfied() => {
        // Expectations an unordered mock passes over stay pending.
        if !unordered {
          skipped.push(*idx);
        }

        rejections.push((candidate, mismatch));
      }

      Err(mismatch) => return Err(Reason::Next(candidate.rejection(mismatch))),
    }
  }

  Err(no_match(total, rejections, if unordered { "pending" } else { "remaining optional" }))
}

/// Tell why no expectation answered a statement:
///
/// - `NoExpectation` when the mock has no expectations at all (`total` is 0);
/// - `Consumed` when none of them was pending, so that no candidate could
///   reject the statement (`rejections` is empty);
/// - `NoneMatched` otherwise, with each candidate and why it rejected the
///   statement. `candidates` names them in messages: `"pending"` or
///   `"remaining optional"`.
///
/// The rejections are only described here, once the statement has failed:
/// describing an expectation can mean tokenizing its SQL, and most
/// rejections are followed by a match, which does not need them.
fn no_match(total: usize, rejections: Vec<(&Expectation, Mismatch)>, candidates: &'static str) -> Reason {
  if total == 0 {
    Reason::NoExpectation
  } else if rejections.is_empty() {
    Reason::Consumed
  } else {
    let rejections = rejections.into_iter().map(|(candidate, mismatch)| candidate.rejection(mismatch)).collect();

    Reason::NoneMatched { candidates, rejections }
  }
}

/// Fail the test with `problem`, by panicking with its headline and its
/// diagnostic.
///
/// When the thread is already panicking, panicking again would abort the
/// process. That happens when a failing test drops a transaction, which rolls
/// it back. Then this returns an error instead, for the code under test to
/// get. The problem was already recorded, and is reported with the others.
fn unexpected(problem: Box<Problem>) -> DbErr {
  if !std::thread::panicking() {
    // A plain headline comes first, for `should_panic` and searches; the
    // rendered diagnostic, possibly in color, gives the details.
    panic!("leadline: {}\n\n{}", problem.headline(), render::render(std::slice::from_ref(problem.as_ref()), render::colors()));
  }

  DbErr::Custom(format!("leadline: {problem}"))
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
