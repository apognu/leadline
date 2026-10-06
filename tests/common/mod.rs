//! Entities shared by the integration tests. Each test file uses part of
//! them only.
#![allow(dead_code)]

pub mod cake;
pub mod schema;

use leadline::MockDb;

/// Run `run`, in which a statement must fail, and return the plain-text
/// messages of the problems `mock` then reports (see `MockError::problems`).
///
/// The failing statement panics with a diagnostic that may be in color, so
/// tests assert on these messages instead. `run` is spawned as a task, so
/// that its panic fails the task rather than the test.
pub async fn problems(mock: &MockDb, run: impl Future<Output = ()> + Send + 'static) -> Vec<String> {
  assert!(tokio::spawn(run).await.is_err(), "no statement failed");

  mock.check().unwrap_err().problems().to_vec()
}
