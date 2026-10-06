use std::{
  ops::Range,
  panic::Location,
  path::{Path, PathBuf},
};

use annotate_snippets::{Annotation, AnnotationKind, Element, Group, Level, Origin, Patch, Renderer, Snippet};
use sea_orm::{DbBackend, Statement, Value};

use crate::{
  classify::StmtKind,
  diff::{self, Change, Interpolated},
  expectation::quote,
  matcher::{Arg, SqlMatcher, StatementExt, sealed::IntoArg, wrong_args},
  parse::parse,
  problem::{Declared, Mismatch, Problem, Reason, Rejection},
};

/// The help given for an expectation without a result.
const ADD_RESULT: &str = "complete it with a result, such as `.returning(..)` or `.rows_affected(..)`";

/// Invisible characters written before and after each bound value when
/// rendering in color, in place of braces.
///
/// They take no room on screen, so the renderer still puts its carets under
/// the right characters. Once the diagnostic is rendered, `highlight` turns
/// them into the codes that color the values.
const OPEN: &str = "\u{2063}";
const CLOSE: &str = "\u{2064}";

/// The terminal codes that make bound values yellow and underlined.
const HIGHLIGHT: &str = "\x1b[33;4m";
/// The terminal code that clears every style.
const RESET: &str = "\x1b[0m";

/// Whether diagnostics should be rendered in color.
///
/// The environment decides first, as for cargo, in this order: `NO_COLOR`
/// disables colors, `CLICOLOR_FORCE` enables them, and `CLICOLOR=0` disables
/// them. Otherwise, colors are used when both stderr and stdout are
/// terminals, and either the terminal supports colors (`TERM` is not
/// `dumb`), `CLICOLOR=1` is set, or the tests run on CI.
///
/// Both streams are checked because a panic message is written to stderr,
/// but the test harness captures it and prints it on stdout, with the output
/// of the failed test: `cargo test > log.txt` must not write color codes into
/// the file.
pub(crate) fn colors() -> bool {
  use std::io::IsTerminal;

  // As `anstream` decides for a stream.
  let clicolor = anstyle_query::clicolor();
  let colors = if anstyle_query::no_color() {
    false
  } else if anstyle_query::clicolor_force() {
    true
  } else if clicolor == Some(false) {
    false
  } else {
    std::io::stderr().is_terminal() && std::io::stdout().is_terminal() && (anstyle_query::term_supports_color() || clicolor == Some(true) || anstyle_query::is_ci())
  };

  // Old Windows consoles need ANSI codes enabled; elsewhere, this does nothing.
  if colors {
    let _ = anstyle_query::windows::enable_ansi_colors();
  }

  colors
}

/// Render `problems` the way the Rust compiler renders its errors. Each
/// problem shows the line of the test that declared the expectation involved.
/// An unexpected statement also shows the statement received, and how it
/// differs from what the expectation accepts, here as a diff:
///
/// ```text
/// error: unexpected SELECT
///   --> tests/cakes.rs:24:6
///    |
/// 24 |     .expect_select::<cake::Entity>()
///    |      ------------------------------- the next expectation
///    |
///  1 - SELECT "cake"."id" FROM "cake" WHERE "cake"."name" = {'Chocolate'}
///  1 + SELECT "cake"."id" FROM "cake" WHERE "cake"."name" LIKE {'%choc%'}
///    |
///    = note: bound values are shown in place of their placeholders, between braces
/// ```
///
/// Bound values are written in place of their placeholders (`$1`, `?`), so
/// that one diff shows both the SQL and the values. To tell them from
/// literals written in the SQL, they are put between braces, or, with
/// `colors`, shown in yellow and underlined instead.
pub(crate) fn render(problems: &[Problem], colors: bool) -> String {
  // Statements are never cut to the width of the terminal. Not `usize::MAX`:
  // the renderer adds to the width, which would overflow.
  let renderer = if colors { Renderer::styled() } else { Renderer::plain() }.term_width(usize::MAX / 2);
  let style = Style { colors };

  let rendered = problems
    .iter()
    .flat_map(|problem| style.reports(problem))
    .map(|report| renderer.render(&[report]))
    .collect::<Vec<_>>()
    .join("\n\n");

  if colors { highlight(&rendered) } else { rendered }
}

/// Replace the invisible markers around bound values (`OPEN` and `CLOSE`) in
/// a rendered diagnostic with the codes that color them.
///
/// After a value, the styles that were active before it are set again. A
/// value can be inside a part of a diff the renderer colors, such as an added
/// ` LIMIT 1 OFFSET 2` in green: clearing every style after the first value
/// would leave the rest of that part uncolored. With each code written in
/// brackets:
///
/// ```text
/// rendered      [green] LIMIT [OPEN]1[CLOSE] OFFSET …[reset]
/// highlighted   [green] LIMIT [yellow, underlined]1[reset][green] OFFSET …[reset]
/// ```
fn highlight(rendered: &str) -> String {
  let mut highlighted = String::with_capacity(rendered.len());
  // The styles set since the last reset, to set again after a value.
  let mut active = String::new();
  let mut rest = rendered;

  while let Some(c) = rest.chars().next() {
    if let Some(after) = rest.strip_prefix(OPEN) {
      highlighted.push_str(HIGHLIGHT);
      rest = after;
    } else if let Some(after) = rest.strip_prefix(CLOSE) {
      highlighted.push_str(RESET);
      highlighted.push_str(&active);
      rest = after;
    } else if c == '\x1b' {
      // A style runs until its final `m`.
      let end = rest.find('m').map_or(rest.len(), |end| end + 1);
      let style = &rest[..end];

      if style == RESET || style == "\x1b[m" {
        active.clear();
      } else {
        active.push_str(style);
      }

      highlighted.push_str(style);
      rest = &rest[end..];
    } else {
      highlighted.push(c);
      rest = &rest[c.len_utf8()..];
    }
  }

  highlighted
}

/// How diagnostics are rendered: with `colors`, bound values are colored,
/// otherwise they are put between braces.
#[derive(Clone, Copy)]
struct Style {
  colors: bool,
}

/// Why an expectation rejected a statement, as the parts of the block that
/// shows the expectation. For a `SELECT` received where a `DELETE` is
/// expected:
///
/// ```text
///  1 | SELECT "cake"."id" FROM "cake"           <- statement
///    | ^^^^^^ this is a SELECT statement
///    |
///   ::: tests/cakes.rs:12:6                    <- the declaration, which the
///    |                                            caller adds
/// 12 |     .expect_delete::<cake::Entity>()
///    |      ------------------------------- the next expectation
///    |
///    = note: expected a DELETE statement       <- after
/// ```
struct Explanation {
  /// The received statement, when there is something to point at in it: its
  /// keyword (`this is a SELECT statement`), its table, or the values that do
  /// not match. It comes before the expectation's declaration.
  ///
  /// When it is `None`, the caller shows the statement without annotations,
  /// unless `diff` shows it already.
  statement: Option<Element<'static>>,
  /// What comes after the expectation's declaration: diffs, notes and help.
  after: Vec<Element<'static>>,
  /// Whether `after` holds a diff of the statement, which then needs not be
  /// shown again: `Some(true)` when the diff shows the values in place of
  /// their placeholders, `Some(false)` when it shows the placeholders, and
  /// `None` without a diff of the statement.
  diff: Option<bool>,
}

impl Style {
  /// Build the reports for one problem. Each report is a block rendered on its
  /// own. There is one `error:` block, followed, when every pending
  /// expectation rejected the statement (`Reason::NoneMatched`), by a
  /// `note: candidate n of m` block for each of the closest ones (see
  /// `none_matched`).
  fn reports(self, problem: &Problem) -> Vec<Group<'static>> {
    match problem {
      Problem::Unexpected { kind, stmt, reason } => self.unexpected(*kind, stmt, reason),

      Problem::Unmet(expectation) => vec![Group::with_title(Level::ERROR.primary_title(format!("expectation not met: {}", expectation.label))).elements(declaration(expectation, "expected here"))],

      Problem::NoResult(expectation) => vec![
        Group::with_title(Level::ERROR.primary_title(format!("expectation has no result: {}", expectation.label)))
          .elements(declaration(expectation, "declared here"))
          .element(Level::HELP.message(ADD_RESULT)),
      ],
    }
  }

  /// Build the reports for a statement that no expectation answered. The
  /// `error:` block shows the statement, then explains why it was not
  /// answered (see `Reason`).
  fn unexpected(self, kind: StmtKind, stmt: &Statement, reason: &Reason) -> Vec<Group<'static>> {
    let error = Group::with_title(Level::ERROR.primary_title(format!("unexpected {kind}")));

    let error = match reason {
      Reason::Next(rejection) => {
        let Explanation { statement, after, diff } = self.mismatch(stmt, &rejection.mismatch);

        // The statement, with something pointed at in it if there is
        // something to point at, and shown plain otherwise, unless a diff
        // shows it already.
        let statement = statement.or_else(|| diff.is_none().then(|| self.received(stmt, no_annotation)));
        let legend = if diff == Some(false) { None } else { self.legend(stmt) };

        return vec![
          error
            .elements(statement)
            .elements(declaration(&rejection.expectation, "the next expectation"))
            .elements(after)
            .elements(legend),
        ];
      }

      Reason::NoExpectation => error.element(self.received(stmt, no_annotation)).element(Level::NOTE.message("no expectation was set")),

      Reason::Consumed => error.element(self.received(stmt, no_annotation)).element(Level::NOTE.message("every expectation was already consumed")),

      Reason::NoneMatched { candidates, rejections } => return self.none_matched(error, stmt, candidates, rejections),

      Reason::MatchedWithoutResult(expectation) => error
        .element(self.received(stmt, no_annotation))
        .elements(declaration(expectation, "matches it, but has no result"))
        .element(Level::HELP.message(ADD_RESULT)),

      Reason::MissingRows { fix } => error
        .element(self.received(stmt, no_annotation))
        .element(Level::NOTE.message("it reads the written rows back (`RETURNING` on this backend), but its expectation has no rows to return"))
        .element(Level::HELP.message(format!("complete the expectation with {fix}"))),
    };

    vec![error.elements(self.legend(stmt))]
  }

  /// Build the reports for a statement that every candidate rejected: an
  /// `error:` block with the statement, then a `note: candidate n of m` block
  /// for each of the candidates closest to it.
  ///
  /// The candidates are put in three groups, by how close they are to the
  /// statement. For a `SELECT` on `cake`:
  ///
  /// 1. those of the same kind and on the same table (a `SELECT` on `cake`,
  ///    or a `SELECT` of `expect_query`, which has no table) are what the
  ///    test most likely meant: each gets a block, with how it differs from
  ///    the statement;
  /// 2. those of the same kind, on another table (a `SELECT` on `bakery`),
  ///    get one line each in the error block;
  /// 3. those of another kind (a `DELETE`) are only counted, in the error
  ///    block.
  ///
  /// When the first group is empty, the second one gets the blocks instead,
  /// and the third one is still counted. When both are empty, the third one
  /// gets one line each. So the report always shows some of what was pending.
  fn none_matched(self, error: Group<'static>, stmt: &Statement, candidates: &str, rejections: &[Rejection]) -> Vec<Group<'static>> {
    let (other_kinds, same_kind): (Vec<_>, Vec<_>) = rejections.iter().partition(|rejection| matches!(rejection.mismatch, Mismatch::Kind { .. }));
    let (other_tables, close): (Vec<_>, Vec<_>) = same_kind.into_iter().partition(|rejection| matches!(rejection.mismatch, Mismatch::Table { .. }));

    let (shown, listed, hidden) = match (close.is_empty(), other_tables.is_empty()) {
      (false, _) => (close, other_tables, other_kinds.len()),
      (true, false) => (other_tables, Vec::new(), other_kinds.len()),
      (true, true) => (Vec::new(), other_kinds, 0),
    };

    let listed = listed.into_iter().map(|rejection| {
      let Declared { label, location } = &rejection.expectation;
      let reason = match rejection.mismatch {
        Mismatch::Kind { .. } => "of another kind",
        // No table could be read from the statement, which does not mention
        // the expected one either.
        Mismatch::Table { actual: None, .. } => "on a table it does not mention",
        _ => "on another table",
      };

      Level::NOTE.message(format!("{reason}: {label} ({location})"))
    });

    let hidden = match hidden {
      0 => None,
      1 => Some(Level::NOTE.message("1 expectation of another kind is not shown")),
      n => Some(Level::NOTE.message(format!("{n} expectations of other kinds are not shown"))),
    };

    let error = error
      .element(self.received(stmt, no_annotation))
      .element(Level::NOTE.message(format!("none of the {} {candidates} expectations matches it", rejections.len())))
      .elements(listed)
      .elements(hidden)
      .elements(self.legend(stmt));

    let count = shown.len();

    let candidates = shown.into_iter().enumerate().map(|(index, rejection)| {
      let Explanation { statement, after, .. } = self.mismatch(stmt, &rejection.mismatch);

      // The error shows the statement already: it is only shown again with
      // something pointed at in it.
      Group::with_title(Level::NOTE.primary_title(format!("candidate {} of {count}", index + 1)))
        .elements(statement)
        .elements(declaration(&rejection.expectation, "declared here"))
        .elements(after)
    });

    std::iter::once(error).chain(candidates).collect()
  }

  /// Explain why an expectation rejected `stmt`, for each kind of mismatch:
  ///
  /// - another kind: the statement's keyword is pointed at, and a note gives
  ///   the expected kind;
  /// - another table: the table is pointed at, when it is known (see
  ///   `Mismatch::Table`), and a note gives the expected one;
  /// - a SQL matcher: a diff from the expected statement for the `matching*`
  ///   methods and `sql(..)`, and a note naming the matcher for the others
  ///   (`expected SQL containing …`). A hint, if any, is added as help;
  /// - the arguments: the values that do not match are pointed at, or the
  ///   lists are compared when they are not as many (see `arguments`).
  fn mismatch(self, stmt: &Statement, mismatch: &Mismatch) -> Explanation {
    let backend = stmt.db_backend;
    let note = |message: String| -> Element<'static> { Level::NOTE.message(message).into() };

    match mismatch {
      Mismatch::Kind { expected, actual } => {
        let label = format!("this is {} {actual} statement", article(*actual));
        let keyword = move |shown: &Interpolated| vec![AnnotationKind::Primary.span(first_word(&shown.text)).label(label)];

        Explanation {
          statement: Some(self.received(stmt, keyword)),
          after: vec![note(format!("expected {} {expected} statement", article(*expected)))],
          diff: None,
        }
      }

      Mismatch::Table { expected, actual, backend } => {
        // Without a table read from the statement (see `Mismatch::Table`),
        // there is nothing to point at.
        let statement = actual.as_deref().map(|actual| {
          let table = quote(*backend, actual);
          let annotate = move |shown: &Interpolated| {
            let span = find_table(&shown.text, &table);

            span.map(|span| AnnotationKind::Primary.span(span).label("on this table")).into_iter().collect()
          };

          self.received(stmt, annotate)
        });

        Explanation {
          statement,
          after: vec![note(format!("expected a statement on `{expected}`"))],
          diff: None,
        }
      }

      Mismatch::Sql { matcher, hint } => {
        let mut explanation = match matcher.as_ref() {
          // The expectation ignores `LIMIT` and `OFFSET`: the statements are
          // compared without them too. When the received statement does not
          // parse, they cannot be removed, and the next arm compares it whole.
          SqlMatcher::Statement {
            ignore_limit: true,
            unpaginated: Some((expected_sql, expected_values)),
            ..
          } if let Some(parsed) = parse(stmt) => {
            let (actual_sql, actual_values) = &parsed.unpaginated;

            let mut explanation = self.compare((expected_sql, Some(expected_values)), (actual_sql, actual_values), backend);
            explanation.after.push(note("LIMIT and OFFSET are ignored".to_string()));
            explanation
          }
          SqlMatcher::Statement { stmt: expected, .. } => self.compare((&expected.sql, Some(expected.args())), (&stmt.sql, stmt.args()), backend),
          SqlMatcher::Exact(expected) => self.compare((expected, None), (&stmt.sql, stmt.args()), backend),
          matcher => Explanation {
            statement: None,
            after: vec![note(format!("expected {matcher}"))],
            diff: None,
          },
        };

        if let Some(hint) = hint {
          explanation.after.push(Level::HELP.message(*hint).into());
        }

        explanation
      }

      Mismatch::Args { expected, actual, wrong } => self.arguments(stmt, expected, actual, wrong),
    }
  }

  /// Explain how the received statement differs from the expected one. Each
  /// statement is given as its SQL and its bound values; `sql(..)` gives no
  /// expected values.
  ///
  /// The two statements are compared with their values written in place of
  /// their placeholders, so that one diff covers the SQL and the values:
  ///
  /// ```text
  ///  1 - SELECT … WHERE "cake"."id" = {1}
  ///  1 + SELECT … WHERE "cake"."id" = {2} LIMIT {1}
  /// ```
  ///
  /// If that diff is empty while the values differ, they read the same but
  /// differ by type, as `1` as an `i32` and as an `i64`: a note says so.
  ///
  /// Without expected values, when values and placeholders do not line up,
  /// or when the statements with their values do not tokenize, the statements
  /// are compared with their placeholders (`= $1`), and the values are
  /// compared apart, as lists (see `values_diff`). When even those statements
  /// do not tokenize, both are shown, one after the other.
  fn compare(self, expected: (&str, Option<&[Value]>), actual: (&str, &[Value]), backend: DbBackend) -> Explanation {
    if let Some(expected_values) = expected.1
      && let Some(expected_shown) = self.show(expected.0, expected_values, backend)
      && let Some(actual_shown) = self.show(actual.0, actual.1, backend)
      && let Some(changes) = diff::sql(&expected_shown.text, &actual_shown.text, backend)
    {
      if !changes.is_empty() {
        return Explanation {
          statement: None,
          after: vec![patched(&expected_shown.text, changes)],
          diff: Some(true),
        };
      }

      if expected_values != actual.1 {
        return Explanation {
          statement: None,
          after: vec![type_note(expected_values, actual.1)],
          diff: None,
        };
      }
    }

    let mut explanation = match diff::sql(expected.0, actual.0, backend) {
      Some(changes) if !changes.is_empty() => Explanation {
        statement: None,
        after: vec![patched(expected.0, changes)],
        diff: Some(false),
      },
      // Only the values differ: they are compared below.
      Some(_) => Explanation {
        statement: None,
        after: Vec::new(),
        diff: None,
      },
      None => Explanation {
        statement: None,
        after: vec![
          plain(expected.0),
          plain(actual.0),
          Level::NOTE.message("the expected statement is first, the received statement second").into(),
        ],
        diff: Some(false),
      },
    };

    if let Some(expected_values) = expected.1
      && expected_values != actual.1
    {
      let expected_args: Vec<Arg> = expected_values.iter().cloned().map(IntoArg::into_arg).collect();
      // Plain values: matching them calls no user code.
      let wrong = wrong_args(&expected_args, actual.1).unwrap_or_default();
      let values = values_diff(&expected_args, actual.1, &wrong, backend);

      explanation.after.extend(if values.is_empty() { vec![type_note(expected_values, actual.1)] } else { values });
    }

    explanation
  }

  /// Explain why the values bound to `stmt` do not match the arguments of
  /// `with_args`. `wrong` holds the positions of the values that do not match
  /// (see `Mismatch::Args`).
  ///
  /// When there are as many values as arguments, the statement is shown with
  /// each wrong value pointed at:
  ///
  /// ```text
  ///  1 | DELETE FROM "cake" WHERE "cake"."id" = {8}
  ///    |                                         ^ expected 7
  /// ```
  ///
  /// Otherwise, or when the values cannot be written in place of their
  /// placeholders, the arguments and the values are compared as lists (see
  /// `values_diff`).
  fn arguments(self, stmt: &Statement, expected: &[Arg], actual: &[Value], wrong: &[usize]) -> Explanation {
    let backend = stmt.db_backend;

    if expected.len() == actual.len()
      && let Some(shown) = self.show(&stmt.sql, actual, backend)
    {
      let annotations: Vec<_> = shown
        .values
        .iter()
        .filter(|(index, _)| wrong.contains(index))
        .map(|(index, range)| AnnotationKind::Primary.span(range.clone()).label(format!("expected {}", diff::display_arg(&expected[*index], backend))))
        .collect();

      return Explanation {
        statement: Some(Snippet::source(shown.text).fold(false).annotations(annotations).into()),
        after: Vec::new(),
        diff: None,
      };
    }

    Explanation {
      statement: None,
      after: values_diff(expected, actual, wrong, backend),
      diff: None,
    }
  }

  /// Show the received statement, with its values in place of their
  /// placeholders when they line up.
  ///
  /// `annotate` gets the text that is shown, and returns what to point at in
  /// it: for example, the table, with the label `on this table`.
  fn received(self, stmt: &Statement, annotate: impl FnOnce(&Interpolated) -> Vec<Annotation<'static>>) -> Element<'static> {
    let shown = self.show(&stmt.sql, stmt.args(), stmt.db_backend).unwrap_or_else(|| Interpolated {
      text: stmt.sql.clone(),
      values: Vec::new(),
    });

    let annotations = annotate(&shown);

    Snippet::source(shown.text).fold(false).annotations(annotations).into()
  }

  /// Write `values` in place of the placeholders of `sql`: between invisible
  /// markers that become colors, or between braces without colors (see
  /// `diff::interpolate`). Returns `None` when they do not line up.
  fn show(self, sql: &str, values: &[Value], backend: DbBackend) -> Option<Interpolated> {
    let literals: Vec<String> = values.iter().map(|value| diff::literal(value, backend)).collect();

    let around = if self.colors { (OPEN, CLOSE) } else { ("{", "}") };

    diff::interpolate(sql, &literals, backend, around)
  }

  /// The note telling how bound values are shown, `bound values are shown in
  /// place of their placeholders, between braces`. Returns `None` when `stmt`
  /// is not shown with values: it has none, or they do not line up with its
  /// placeholders.
  fn legend(self, stmt: &Statement) -> Option<Element<'static>> {
    let shown = self.show(&stmt.sql, stmt.args(), stmt.db_backend)?;

    let how = if self.colors { "in color" } else { "between braces" };

    (!shown.values.is_empty()).then(|| Level::NOTE.message(format!("bound values are shown in place of their placeholders, {how}")).into())
  }
}

/// A note for values that read the same but differ by type, such as `1` as an
/// `i32` and as an `i64`: it shows both lists with their types,
/// `expected [Int(Some(1))], received [BigInt(Some(1))]`.
fn type_note(expected: &[Value], actual: &[Value]) -> Element<'static> {
  Level::NOTE.message(format!("the values differ by type: expected {expected:?}, received {actual:?}")).into()
}

/// Compare the bound values to the expected arguments as lists, with a note
/// counting both:
///
/// ```text
///  1 - [7, _]
///  1 + [8]
///    |
///    = note: expected 2 arguments, received 1 value
/// ```
///
/// `wrong` holds the positions of the values that do not match, when there
/// are as many values as arguments (see `diff::args`). Returns nothing when
/// there is no change to show: with as many values as arguments, that is
/// when `wrong` is empty.
fn values_diff(expected: &[Arg], actual: &[Value], wrong: &[usize], backend: DbBackend) -> Vec<Element<'static>> {
  let (expected_list, _, changes) = diff::args(expected, actual, wrong, backend);

  if changes.is_empty() {
    return Vec::new();
  }

  let count = |n: usize, noun: &str| if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") };
  let note = format!("expected {}, received {}", count(expected.len(), "argument"), count(actual.len(), "value"));

  vec![patched(&expected_list, changes), Level::NOTE.message(note).into()]
}

/// Show the diff that `changes` make to `text`: the lines before and after,
/// marked `-` and `+`, or `+` markers under what is inserted.
fn patched(text: &str, changes: Vec<Change>) -> Element<'static> {
  let patches = changes.into_iter().map(|change| Patch::new(change.expected, change.replacement));

  Snippet::source(text.to_string()).patches(patches).into()
}

/// An `annotate` function for `Style::received` that points at nothing.
fn no_annotation(_: &Interpolated) -> Vec<Annotation<'static>> {
  Vec::new()
}

/// Show `text` as it is, without annotations.
fn plain(text: &str) -> Element<'static> {
  Snippet::<Annotation>::source(text.to_string()).fold(false).into()
}

/// Show the line of the test that declared an expectation, with the call
/// underlined and labeled `label`:
///
/// ```text
///   ::: tests/cakes.rs:24:6
///    |
/// 24 |     .expect_select::<cake::Entity>()
///    |      ------------------------------- the next expectation
/// ```
///
/// When the test's file cannot be read, only its path, line and column are
/// shown, with a note describing the expectation instead.
fn declaration(expectation: &Declared, label: &str) -> Vec<Element<'static>> {
  let location: &Location<'static> = expectation.location;

  let Some(line) = source_line(location) else {
    return vec![
      Origin::path(location.file()).line(location.line() as usize).char_column(location.column() as usize).into(),
      Level::NOTE.message(format!("{label}: {}", expectation.label)).into(),
    ];
  };

  // Columns count characters, from 1.
  let start = line.char_indices().nth(location.column().saturating_sub(1) as usize).map_or(0, |(offset, _)| offset);
  let end = line.trim_end().len().max(start);

  vec![
    Snippet::source(line)
      .line_start(location.line() as usize)
      .path(location.file())
      .annotation(AnnotationKind::Context.span(start..end).label(label.to_string()))
      .into(),
  ]
}

/// Read the line of source at `location`. Returns `None` when its file cannot
/// be found (see `read_source`).
fn source_line(location: &Location<'_>) -> Option<String> {
  let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
  let source = read_source(location.file(), manifest_dir.as_deref())?;

  source.lines().nth(location.line().checked_sub(1)? as usize).map(str::to_string)
}

/// Read the source file `file`, a path as `Location::file` gives it.
///
/// The path is relative to the root of the workspace, which cargo compiles
/// from, while tests run from their crate's directory. Both are the same for
/// a crate on its own, but not in a workspace: for `my-workspace/crates/app`,
/// a test of `app` runs from `crates/app`, and sees its own file as
/// `crates/app/tests/cakes.rs`. So the file is looked for from the current
/// directory, then from `manifest_dir` (the crate's directory) and each of
/// its parents, up to the root of the workspace and beyond.
fn read_source(file: &str, manifest_dir: Option<&Path>) -> Option<String> {
  let parents = manifest_dir.into_iter().flat_map(Path::ancestors);

  std::iter::once(Path::new("")).chain(parents).find_map(|dir| std::fs::read_to_string(dir.join(file)).ok())
}

/// The article that goes before a statement kind: "an" for `INSERT` and
/// `UPDATE`, "a" for `SELECT` and the others.
fn article(kind: StmtKind) -> &'static str {
  if matches!(kind, StmtKind::Insert | StmtKind::Update) { "an" } else { "a" }
}

/// The byte range of the first word of `sql`, its keyword: `0..6` for
/// `SELECT "id" FROM "cake"`.
fn first_word(sql: &str) -> Range<usize> {
  let start = sql.len() - sql.trim_start().len();
  let end = sql[start..].find(char::is_whitespace).map_or(sql.len(), |end| start + end);

  start..end
}

/// Find where `sql` names `table` (quoted, as `"cake"`) as a table, to point
/// at it.
///
/// In `SELECT "cake"."id" FROM "cake"`, the first `"cake"` only qualifies a
/// column: the one after `FROM` is the table. So the first occurrence that is
/// not followed by a `.` is returned. When every occurrence is followed by a
/// `.`, the first one is returned instead. Returns `None` when `sql` does not
/// contain `table`.
///
/// This is a plain text search: a string value containing `"cake"` would be
/// found too.
fn find_table(sql: &str, table: &str) -> Option<Range<usize>> {
  let mut occurrences = sql.match_indices(table).map(|(start, _)| start..start + table.len()).peekable();
  let first = occurrences.peek().cloned();

  occurrences.find(|range| !sql[range.end..].starts_with('.')).or(first)
}

#[cfg(test)]
mod tests {
  // Not every entity is used here, and the entities are public, as the
  // doctests they are written for need.
  #![allow(dead_code, unreachable_pub)]

  use std::panic::{AssertUnwindSafe, catch_unwind};

  use regex::Regex;
  use sea_orm::{ActiveModelTrait, ActiveValue::Set, DatabaseConnection, DbBackend, EntityTrait, Statement};

  use super::render;
  use crate::MockDb;

  include!("../doctests/entities.rs");

  /// What the mock reports after `run` sends statements to it, rendered
  /// without colors. Locations in this file are replaced by `LL:CC`, and the
  /// gutter with its line numbers is dropped, so that the expected reports do
  /// not change when tests are added.
  fn report(mock: MockDb, run: impl AsyncFnOnce(DatabaseConnection)) -> String {
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();

    // Unexpected statements panic; the mock reports them again below.
    let _ = catch_unwind(AssertUnwindSafe(|| runtime.block_on(async { run(mock.connection().await).await })));

    let rendered = render(&mock.check().unwrap_err().problems, false);
    let rendered = Regex::new(r"src/render\.rs:\d+:\d+").unwrap().replace_all(&rendered, "src/render.rs:LL:CC");

    // The gutter's width follows the line numbers: drop it, keeping what
    // follows, whose columns stay aligned.
    Regex::new(r"(?m)^[ \d]*(-->|:::|\||=|- |\+ )").unwrap().replace_all(&rendered, "$1").into_owned()
  }

  fn find_cake(id: i32) -> impl AsyncFnOnce(DatabaseConnection) {
    async move |db| {
      let _ = cake::Entity::find_by_id(id).one(&db).await;
    }
  }

  #[test]
  fn statement_and_arguments_differ() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(1)).returning::<cake::Model>([]);

    assert_eq!(
      report(mock, find_cake(1)),
      r#"
error: unexpected SELECT
--> src/render.rs:LL:CC
|
|     mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(1)).returning::<cake::Model>([]);
|          --------------------------------------------------------------------------------------------------- the next expectation
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|                                                                                           +++++++++
= help: they only differ by LIMIT/OFFSET, which `.one()` and paginators add: use `matching_ignoring_limit`
= note: bound values are shown in place of their placeholders, between braces

error: expectation not met: SELECT on `cake` with statement `SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = $1` with [Int(Some(1))]
--> src/render.rs:LL:CC
|
|     mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(1)).returning::<cake::Model>([]);
|          --------------------------------------------------------------------------------------------------- expected here"#
        .trim_start()
    );
  }

  #[test]
  fn arguments_differ() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_delete::<cake::Entity>().with_args((7, crate::Any)).rows_affected(1);

    let output = report(mock, async |db| {
      let _ = cake::Entity::delete_by_id(8).exec(&db).await;
    });

    assert_eq!(
      output,
      r#"
error: unexpected DELETE
|
| DELETE FROM "cake" WHERE "cake"."id" = {8}
|
::: src/render.rs:LL:CC
|
|     mock.expect_delete::<cake::Entity>().with_args((7, crate::Any)).rows_affected(1);
|          ---------------------------------------------------------------------------- the next expectation
|
- [7, _]
+ [8]
|
= note: expected 2 arguments, received 1 value
= note: bound values are shown in place of their placeholders, between braces

error: expectation not met: DELETE on `cake` with any SQL and args [Int(Some(7)), <any>]
--> src/render.rs:LL:CC
|
|     mock.expect_delete::<cake::Entity>().with_args((7, crate::Any)).rows_affected(1);
|          ---------------------------------------------------------------------------- expected here"#
        .trim_start()
    );
  }

  #[test]
  fn sql_differs() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_select::<cake::Entity>().sql_contains("ORDER BY").returning::<cake::Model>([]);

    assert_eq!(
      report(mock, find_cake(1)),
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|
::: src/render.rs:LL:CC
|
|     mock.expect_select::<cake::Entity>().sql_contains("ORDER BY").returning::<cake::Model>([]);
|          -------------------------------------------------------------------------------------- the next expectation
|
= note: expected SQL containing `ORDER BY`
= note: bound values are shown in place of their placeholders, between braces

error: expectation not met: SELECT on `cake` with SQL containing `ORDER BY`
--> src/render.rs:LL:CC
|
|     mock.expect_select::<cake::Entity>().sql_contains("ORDER BY").returning::<cake::Model>([]);
|          -------------------------------------------------------------------------------------- expected here"#
        .trim_start()
    );
  }

  #[test]
  fn kind_differs() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_delete::<cake::Entity>().rows_affected(1);

    assert_eq!(
      report(mock, find_cake(1)),
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
| ^^^^^^ this is a SELECT statement
|
::: src/render.rs:LL:CC
|
|     mock.expect_delete::<cake::Entity>().rows_affected(1);
|          ------------------------------------------------- the next expectation
|
= note: expected a DELETE statement
= note: bound values are shown in place of their placeholders, between braces

error: expectation not met: DELETE on `cake` with any SQL
--> src/render.rs:LL:CC
|
|     mock.expect_delete::<cake::Entity>().rows_affected(1);
|          ------------------------------------------------- expected here"#
        .trim_start()
    );
  }

  #[test]
  fn table_differs() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_select::<bakery::Entity>().returning::<bakery::Model>([]);

    assert_eq!(
      report(mock, find_cake(1)),
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|                                                            ^^^^^^ on this table
|
::: src/render.rs:LL:CC
|
|     mock.expect_select::<bakery::Entity>().returning::<bakery::Model>([]);
|          ----------------------------------------------------------------- the next expectation
|
= note: expected a statement on `bakery`
= note: bound values are shown in place of their placeholders, between braces

error: expectation not met: SELECT on `bakery` with any SQL
--> src/render.rs:LL:CC
|
|     mock.expect_select::<bakery::Entity>().returning::<bakery::Model>([]);
|          ----------------------------------------------------------------- expected here"#
        .trim_start()
    );
  }

  #[test]
  fn no_expectation() {
    assert_eq!(
      report(MockDb::new(DbBackend::Postgres), find_cake(1)),
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|
= note: no expectation was set
= note: bound values are shown in place of their placeholders, between braces"#
        .trim_start()
    );
  }

  #[test]
  fn every_expectation_consumed() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_select::<cake::Entity>().returning::<cake::Model>([]);

    let output = report(mock, async |db| {
      let _ = cake::Entity::find_by_id(1).one(&db).await;
      let _ = cake::Entity::find_by_id(2).one(&db).await;
    });

    assert_eq!(
      output,
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {2} LIMIT {1}
|
= note: every expectation was already consumed
= note: bound values are shown in place of their placeholders, between braces"#
        .trim_start()
    );
  }

  #[test]
  fn no_candidate_matches() {
    let mock = MockDb::new(DbBackend::Postgres).unordered();
    mock.expect_delete::<cake::Entity>().rows_affected(1);
    mock.expect_select::<cake::Entity>().sql_contains("ORDER BY").returning::<cake::Model>([]);

    assert_eq!(
      report(mock, find_cake(1)),
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|
= note: none of the 2 pending expectations matches it
= note: 1 expectation of another kind is not shown
= note: bound values are shown in place of their placeholders, between braces

note: candidate 1 of 1
--> src/render.rs:LL:CC
|
|     mock.expect_select::<cake::Entity>().sql_contains("ORDER BY").returning::<cake::Model>([]);
|          -------------------------------------------------------------------------------------- declared here
|
= note: expected SQL containing `ORDER BY`

error: expectation not met: DELETE on `cake` with any SQL
--> src/render.rs:LL:CC
|
|     mock.expect_delete::<cake::Entity>().rows_affected(1);
|          ------------------------------------------------- expected here

error: expectation not met: SELECT on `cake` with SQL containing `ORDER BY`
--> src/render.rs:LL:CC
|
|     mock.expect_select::<cake::Entity>().sql_contains("ORDER BY").returning::<cake::Model>([]);
|          -------------------------------------------------------------------------------------- expected here"#
        .trim_start()
    );
  }

  #[test]
  fn closest_candidates_first() {
    let mock = MockDb::new(DbBackend::Postgres).unordered();
    mock.expect_delete::<cake::Entity>().rows_affected(1);
    mock.expect_select::<bakery::Entity>().returning::<bakery::Model>([]);
    mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(2)).returning::<cake::Model>([]);
    mock.expect_select::<cake::Entity>().matching_ignoring_limit(cake::Entity::find()).returning::<cake::Model>([]);

    let output = report(mock, find_cake(1));
    let unexpected = output.split("\n\nerror: expectation not met").next().unwrap();

    assert_eq!(
      unexpected,
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|
= note: none of the 4 pending expectations matches it
= note: on another table: SELECT on `bakery` with any SQL (src/render.rs:LL:CC)
= note: 1 expectation of another kind is not shown
= note: bound values are shown in place of their placeholders, between braces

note: candidate 1 of 2
--> src/render.rs:LL:CC
|
|     mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(2)).returning::<cake::Model>([]);
|          --------------------------------------------------------------------------------------------------- declared here
|
- SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {2}
+ SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|

note: candidate 2 of 2
--> src/render.rs:LL:CC
|
|     mock.expect_select::<cake::Entity>().matching_ignoring_limit(cake::Entity::find()).returning::<cake::Model>([]);
|          ----------------------------------------------------------------------------------------------------------- declared here
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1}
|                                                                   +++++++++++++++++++++++
= note: LIMIT and OFFSET are ignored"#
        .trim_start()
    );
  }

  #[test]
  fn only_candidates_of_other_kinds() {
    let mock = MockDb::new(DbBackend::Postgres).unordered();
    mock.expect_delete::<cake::Entity>().rows_affected(1);
    mock.expect_insert::<cake::Entity>().rows_affected(1);

    let output = report(mock, find_cake(1));
    let unexpected = output.split("\n\nerror: expectation not met").next().unwrap();

    assert_eq!(
      unexpected,
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|
= note: none of the 2 pending expectations matches it
= note: of another kind: DELETE on `cake` with any SQL (src/render.rs:LL:CC)
= note: of another kind: INSERT on `cake` with any SQL (src/render.rs:LL:CC)
= note: bound values are shown in place of their placeholders, between braces"#
        .trim_start()
    );
  }

  #[test]
  fn expectation_without_result() {
    let mock = MockDb::new(DbBackend::Postgres);
    let _ = mock.expect_select::<cake::Entity>();

    assert_eq!(
      report(mock, find_cake(1)),
      r#"
error: unexpected SELECT
|
| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1} LIMIT {1}
|
::: src/render.rs:LL:CC
|
|     let _ = mock.expect_select::<cake::Entity>();
|                  -------------------------------- matches it, but has no result
|
= help: complete it with a result, such as `.returning(..)` or `.rows_affected(..)`
= note: bound values are shown in place of their placeholders, between braces

error: expectation has no result: SELECT on `cake` with any SQL
--> src/render.rs:LL:CC
|
|     let _ = mock.expect_select::<cake::Entity>();
|                  -------------------------------- declared here
|
= help: complete it with a result, such as `.returning(..)` or `.rows_affected(..)`"#
        .trim_start()
    );
  }

  #[test]
  fn missing_returning_rows() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_insert::<bakery::Entity>().rows_affected(1);

    let output = report(mock, async |db| {
      let bakery = bakery::ActiveModel {
        name: Set("Bakery".into()),
        ..Default::default()
      };

      let _ = bakery.insert(&db).await;
    });

    assert_eq!(
      output,
      r#"
error: unexpected INSERT
|
| INSERT INTO "bakery" ("name") VALUES ({'Bakery'}) RETURNING "id", "name"
|
= note: it reads the written rows back (`RETURNING` on this backend), but its expectation has no rows to return
= help: complete the expectation with `.returning(..)`, `.last_insert_id(..)` or `.last_insert_key(..)`
= note: bound values are shown in place of their placeholders, between braces"#
        .trim_start()
    );
  }

  #[test]
  fn unmet_expectation() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_delete::<cake::Entity>().with_args((1,)).rows_affected(1);

    assert_eq!(
      report(mock, async |_| {}),
      r#"
error: expectation not met: DELETE on `cake` with any SQL and args [Int(Some(1))]
--> src/render.rs:LL:CC
|
|     mock.expect_delete::<cake::Entity>().with_args((1,)).rows_affected(1);
|          ----------------------------------------------------------------- expected here"#
        .trim_start()
    );
  }

  #[test]
  fn table_position() {
    let sql = r#"SELECT "cake"."id" FROM "cake" WHERE "cake"."id" = 1"#;
    assert_eq!(super::find_table(sql, r#""cake""#), Some(24..30));
    assert_eq!(super::find_table(r#"SELECT "cake"."id""#, r#""cake""#), Some(7..13));
    assert_eq!(super::find_table(sql, r#""bakery""#), None);
  }

  #[test]
  fn source_in_workspace() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let package = manifest_dir.file_name().unwrap().to_str().unwrap();

    // In a workspace, files are relative to the workspace root, a parent of
    // the package's directory.
    let file = format!("{package}/src/render.rs");
    assert!(std::fs::metadata(&file).is_err());
    assert!(super::read_source(&file, Some(manifest_dir)).unwrap().contains("fn source_in_workspace"));

    assert!(super::read_source("src/render.rs", None).is_some());
    assert!(super::read_source("src/missing.rs", Some(manifest_dir)).is_none());
  }

  #[test]
  fn predicates_are_not_called_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    let mock = MockDb::new(DbBackend::Postgres);
    let predicate = crate::Arg::matching(|_| {
      CALLS.fetch_add(1, Ordering::SeqCst);
      false
    });
    mock.expect_delete::<cake::Entity>().with_args((predicate,)).rows_affected(1);

    let output = report(mock, async |db| {
      let _ = cake::Entity::delete_by_id(1).exec(&db).await;
    });

    assert!(output.contains("^ expected <predicate>"), "{output}");
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn values_differing_by_type() {
    use sea_orm::{ColumnTrait, QueryFilter};

    let mock = MockDb::new(DbBackend::Postgres);
    mock
      .expect_select::<cake::Entity>()
      .matching(cake::Entity::find().filter(cake::Column::Id.eq(1)))
      .returning::<cake::Model>([]);

    let output = report(mock, async |db| {
      let _ = cake::Entity::find().filter(cake::Column::Id.eq(1i64)).all(&db).await;
    });

    // The values read the same: their types tell them apart.
    assert!(
      output.contains(r#"| SELECT "cake"."id", "cake"."name", "cake"."bakery_id" FROM "cake" WHERE "cake"."id" = {1}"#),
      "{output}"
    );
    assert!(output.contains("= note: the values differ by type: expected [Int(Some(1))], received [BigInt(Some(1))]"), "{output}");
  }

  #[test]
  fn literal_against_placeholder() {
    let mock = MockDb::new(DbBackend::Postgres);
    let expected = Statement::from_string(DbBackend::Postgres, r#"DELETE FROM "cake" WHERE "cake"."id" = 7"#);
    mock.expect_statement().matching_statement(expected).rows_affected(1);

    let output = report(mock, async |db| {
      let _ = cake::Entity::delete_by_id(7).exec(&db).await;
    });

    // The bound value is between braces: it differs from the literal.
    assert!(
      output.contains("| DELETE FROM \"cake\" WHERE \"cake\".\"id\" = {7}\n|                                        + +"),
      "{output}"
    );
  }

  #[test]
  fn literal_against_placeholder_in_color() {
    let mock = MockDb::new(DbBackend::Postgres);
    let expected = Statement::from_string(DbBackend::Postgres, r#"DELETE FROM "cake" WHERE "cake"."id" = 7"#);
    mock.expect_statement().matching_statement(expected).rows_affected(1);

    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let _ = catch_unwind(AssertUnwindSafe(|| {
      runtime.block_on(async {
        let db = mock.connection().await;
        let _ = cake::Entity::delete_by_id(7).exec(&db).await;
      })
    }));

    let rendered = render(&mock.check().unwrap_err().problems[..1], true);

    // The bound value, in color, still differs from the literal.
    assert!(rendered.contains("= \x1b[91m7\x1b[0m\n"), "{rendered:?}");
    assert!(rendered.contains("= \x1b[92m\x1b[33;4m7\x1b[0m"), "{rendered:?}");
    assert!(!rendered.contains([super::OPEN, super::CLOSE].map(|marker| marker.chars().next().unwrap())), "{rendered:?}");
  }

  #[test]
  fn highlight_restores_styles() {
    let green = "\x1b[92m";
    let rendered = format!("{green}LIMIT {}1{} OFFSET\x1b[0m", super::OPEN, super::CLOSE);

    // After the value, the rest of the green part is green again.
    assert_eq!(super::highlight(&rendered), format!("{green}LIMIT \x1b[33;4m1\x1b[0m{green} OFFSET\x1b[0m"));
  }

  #[test]
  fn colors() {
    let mock = MockDb::new(DbBackend::Postgres);
    mock.expect_select::<cake::Entity>().matching(cake::Entity::find_by_id(1)).returning::<cake::Model>([]);

    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let panic = catch_unwind(AssertUnwindSafe(|| runtime.block_on(find_cake(2)(runtime.block_on(mock.connection()))))).unwrap_err();
    let message = panic.downcast_ref::<String>().unwrap();

    // The first line stays plain for `should_panic` and log searches.
    assert!(message.starts_with("leadline: unexpected SELECT"));
    assert!(!message.lines().next().unwrap().contains('\x1b'));

    assert!(render(&mock.check().unwrap_err().problems, true).contains("\x1b["));
  }
}
