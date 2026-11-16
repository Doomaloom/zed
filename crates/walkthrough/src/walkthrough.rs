use std::{ops::Range, sync::Arc};

use collections::{HashMap, HashSet};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task,
    WeakEntity, actions,
};
use language::{Buffer, BufferId, BufferSnapshot, Point, ToOffset as _, ToPoint as _};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MIN_STEPS: usize = 1;
pub const MAX_STEPS: usize = 30;
pub const MAX_STEP_LINES: u32 = 60;
pub const ANCHOR_TEXT_SEARCH_RADIUS: u32 = 20;

actions!(
    walkthrough,
    [
        /// Moves to the next step of the active code walkthrough.
        NextStep,
        /// Moves to the previous step of the active code walkthrough.
        PreviousStep,
        /// Opens or closes the flowchart for the active code walkthrough.
        ToggleFlowchart,
        /// Removes the active code walkthrough and all of its annotations.
        Dismiss,
        /// Loads a code walkthrough from a JSON file.
        LoadFromJson,
    ]
);

/// Jumps to a specific step of the active code walkthrough.
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = walkthrough)]
#[serde(deny_unknown_fields)]
pub struct GoToStep {
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WalkthroughId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StepId(pub SharedString);

impl StepId {
    pub fn new(id: impl Into<SharedString>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for StepId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub struct Walkthrough {
    pub id: WalkthroughId,
    pub title: SharedString,
    pub summary: SharedString,
    pub steps: Vec<WalkthroughStep>,
    pub flowchart: Option<Flowchart>,
}

impl Walkthrough {
    pub fn step_index(&self, step: &StepId) -> Option<usize> {
        self.steps
            .iter()
            .position(|candidate| &candidate.id == step)
    }

    pub fn file_count(&self) -> usize {
        self.steps
            .iter()
            .map(|step| &step.path)
            .collect::<HashSet<_>>()
            .len()
    }
}

#[derive(Clone)]
pub struct WalkthroughStep {
    pub id: StepId,
    pub path: ProjectPath,
    pub buffer: Entity<Buffer>,
    pub range: Range<language::Anchor>,
    pub title: SharedString,
    pub comment: SharedString,
    pub node_id: Option<SharedString>,
}

impl WalkthroughStep {
    /// A step is stale once the code it annotated has been deleted, which
    /// collapses its anchors onto the same position.
    pub fn is_stale(&self, cx: &App) -> bool {
        // Compared by offset because anchors into deleted text still order
        // by where that text used to be.
        let snapshot = self.buffer.read(cx);
        self.range.start.to_offset(snapshot) >= self.range.end.to_offset(snapshot)
    }

    pub fn point_range(&self, cx: &App) -> Range<Point> {
        let snapshot = self.buffer.read(cx);
        self.range.start.to_point(snapshot)..self.range.end.to_point(snapshot)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flowchart {
    pub mermaid: SharedString,
    pub node_ids: Vec<SharedString>,
}

impl Flowchart {
    pub fn new(mermaid: impl Into<SharedString>) -> Self {
        let mermaid = mermaid.into();
        let node_ids = parse_flowchart_node_ids(&mermaid);
        Self { mermaid, node_ids }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkthroughEvent {
    Created(WalkthroughId),
    ActiveStepChanged { step: StepId },
    Dismissed,
}

/// Show the user a code walkthrough: an ordered set of comments pinned to
/// highlighted ranges of project files, optionally with a flowchart of the
/// control or data flow. The files are not modified.
///
/// - Use this when the user asks to explain, walk through or understand code. Do not use it for questions answerable in a sentence.
/// - Read every file you annotate first. Take line numbers from what you read.
/// - Order steps by execution or data flow, not by file.
/// - Use 3 to 12 steps. Each comment says why the code exists or what to notice. It never restates the code line by line.
/// - Keep ranges tight: the function signature or the 5 to 25 lines that matter.
/// - Include a flowchart when the code has branching or several stages. Use `flowchart TD`, at most 15 nodes, simple alphanumeric node ids that are not Mermaid keywords (such as `end`, `click`, `style`, `class`, `graph` or `flowchart`), and link each step to a node where one fits.
/// - After the walkthrough is shown, reply with one or two sentences. Do not repeat the explanation in chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalkthroughJson {
    /// A short title for the walkthrough.
    pub title: String,
    /// A markdown summary of the walkthrough, 1 to 3 sentences.
    pub summary: String,
    /// The steps of the walkthrough, in reading order. Between 1 and 30 steps.
    pub steps: Vec<WalkthroughStepJson>,
    /// Mermaid source for a flowchart of the code being explained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flowchart: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalkthroughStepJson {
    /// A short slug identifying this step, unique within the walkthrough.
    pub id: String,
    /// The path of the file to annotate, relative to the project root or
    /// starting with the project's root directory name.
    pub path: String,
    /// The first line of the annotated range (1-based, inclusive).
    pub start_line: u32,
    /// The last line of the annotated range (1-based, inclusive). A range may span at most 60 lines.
    pub end_line: u32,
    /// A short string that appears on the first line of the range. When the
    /// line numbers are slightly off, it is used to find the intended range
    /// within 20 lines either side of `start_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_text: Option<String>,
    /// A short title for this step.
    pub title: String,
    /// A markdown explanation of why this code exists and what to notice.
    pub comment: String,
    /// The id of the flowchart node this step corresponds to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepError {
    /// The id of the step the error belongs to, or `None` when it concerns
    /// the walkthrough as a whole.
    pub step: Option<String>,
    pub kind: StepErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepErrorKind {
    StepCount {
        count: usize,
    },
    DuplicateId,
    PathNotFound {
        path: String,
    },
    OpenFailed {
        path: String,
        message: String,
    },
    InvalidRange {
        start_line: u32,
        end_line: u32,
        line_count: u32,
    },
    RangeTooLong {
        start_line: u32,
        end_line: u32,
    },
    AnchorTextNotFound {
        anchor_text: String,
        start_line: u32,
    },
    InvalidFlowchart {
        message: String,
    },
    UnknownNode {
        node: String,
    },
}

impl std::fmt::Display for StepError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(step) = &self.step {
            write!(formatter, "Step `{step}`: ")?;
        }
        match &self.kind {
            StepErrorKind::StepCount { count } => write!(
                formatter,
                "a walkthrough needs between {MIN_STEPS} and {MAX_STEPS} steps, but {count} were given"
            ),
            StepErrorKind::DuplicateId => {
                write!(formatter, "this id is used by more than one step")
            }
            StepErrorKind::PathNotFound { path } => {
                write!(formatter, "`{path}` is not a file in the project")
            }
            StepErrorKind::OpenFailed { path, message } => {
                write!(formatter, "`{path}` could not be opened: {message}")
            }
            StepErrorKind::InvalidRange {
                start_line,
                end_line,
                line_count,
            } => write!(
                formatter,
                "lines {start_line}-{end_line} are not a valid range in a file with {line_count} lines (lines are 1-based and inclusive)"
            ),
            StepErrorKind::RangeTooLong {
                start_line,
                end_line,
            } => write!(
                formatter,
                "lines {start_line}-{end_line} span more than {MAX_STEP_LINES} lines"
            ),
            StepErrorKind::AnchorTextNotFound {
                anchor_text,
                start_line,
            } => write!(
                formatter,
                "anchor_text `{anchor_text}` was not found within {ANCHOR_TEXT_SEARCH_RADIUS} lines of line {start_line}; re-read the file and correct the line numbers"
            ),
            StepErrorKind::InvalidFlowchart { message } => {
                write!(formatter, "the flowchart could not be parsed: {message}")
            }
            StepErrorKind::UnknownNode { node } => {
                write!(formatter, "`{node}` is not a node id in the flowchart")
            }
        }
    }
}

impl Walkthrough {
    /// Validates `json` against the project and resolves it into a
    /// [`Walkthrough`]. All problems are collected so they can be fixed in a
    /// single retry.
    ///
    /// `check_flowchart` is supplied by the caller so that this crate does
    /// not depend on the Mermaid renderer. It runs on a background thread.
    pub fn from_json(
        json: WalkthroughJson,
        project: &Entity<Project>,
        check_flowchart: impl FnOnce(&str) -> Result<(), String> + Send + 'static,
        cx: &mut App,
    ) -> Task<Result<Walkthrough, Vec<StepError>>> {
        let mut errors = Vec::new();

        if json.steps.len() < MIN_STEPS || json.steps.len() > MAX_STEPS {
            errors.push(StepError {
                step: None,
                kind: StepErrorKind::StepCount {
                    count: json.steps.len(),
                },
            });
        }

        let mut seen_ids = HashSet::default();
        let mut reported_duplicates = HashSet::default();
        for step in &json.steps {
            if !seen_ids.insert(step.id.as_str()) && reported_duplicates.insert(step.id.as_str()) {
                errors.push(StepError {
                    step: Some(step.id.clone()),
                    kind: StepErrorKind::DuplicateId,
                });
            }
        }

        let open_buffers = json
            .steps
            .iter()
            .map(|step| {
                let project_path =
                    project
                        .read(cx)
                        .find_project_path(&step.path, cx)
                        .filter(|project_path| {
                            project
                                .read(cx)
                                .entry_for_path(project_path, cx)
                                .is_some_and(|entry| entry.is_file())
                        });
                project_path.map(|project_path| {
                    let open_buffer = project.update(cx, |project, cx| {
                        project.open_buffer(project_path.clone(), cx)
                    });
                    (project_path, open_buffer)
                })
            })
            .collect::<Vec<_>>();

        cx.spawn(async move |cx| {
            let mut steps = Vec::new();
            for (step_json, open_buffer) in json.steps.iter().zip(open_buffers) {
                let Some((project_path, open_buffer)) = open_buffer else {
                    errors.push(StepError {
                        step: Some(step_json.id.clone()),
                        kind: StepErrorKind::PathNotFound {
                            path: step_json.path.clone(),
                        },
                    });
                    continue;
                };
                let buffer = match open_buffer.await {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        errors.push(StepError {
                            step: Some(step_json.id.clone()),
                            kind: StepErrorKind::OpenFailed {
                                path: step_json.path.clone(),
                                message: error.to_string(),
                            },
                        });
                        continue;
                    }
                };
                let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
                match resolve_step_range(step_json, &snapshot) {
                    Ok(range) => steps.push(WalkthroughStep {
                        id: StepId::new(step_json.id.clone()),
                        path: project_path,
                        buffer,
                        range,
                        title: step_json.title.clone().into(),
                        comment: step_json.comment.clone().into(),
                        node_id: step_json.node.clone().map(SharedString::from),
                    }),
                    Err(kind) => errors.push(StepError {
                        step: Some(step_json.id.clone()),
                        kind,
                    }),
                }
            }

            let flowchart = match json.flowchart {
                Some(source) => {
                    let check = cx
                        .background_spawn({
                            let source = source.clone();
                            async move { check_flowchart(&source) }
                        })
                        .await;
                    match check {
                        Ok(()) => Some(Ok(Flowchart::new(source))),
                        Err(message) => {
                            errors.push(StepError {
                                step: None,
                                kind: StepErrorKind::InvalidFlowchart { message },
                            });
                            Some(Err(()))
                        }
                    }
                }
                None => None,
            };
            // Node ids cannot be trusted when the flowchart does not parse,
            // and reporting every node as unknown would bury the real error.
            let flowchart_is_valid = !matches!(flowchart, Some(Err(())));
            let flowchart = flowchart.and_then(|flowchart| flowchart.ok());

            for step_json in json.steps.iter().filter(|_| flowchart_is_valid) {
                let Some(node) = &step_json.node else {
                    continue;
                };
                let is_known = flowchart.as_ref().is_some_and(|flowchart| {
                    flowchart
                        .node_ids
                        .iter()
                        .any(|node_id| node_id.as_ref() == node.as_str())
                });
                if !is_known {
                    errors.push(StepError {
                        step: Some(step_json.id.clone()),
                        kind: StepErrorKind::UnknownNode { node: node.clone() },
                    });
                }
            }

            if errors.is_empty() {
                Ok(Walkthrough {
                    // The store mints the real id in `set_walkthrough`.
                    id: WalkthroughId(0),
                    title: json.title.into(),
                    summary: json.summary.into(),
                    steps,
                    flowchart,
                })
            } else {
                Err(errors)
            }
        })
    }
}

fn resolve_step_range(
    step: &WalkthroughStepJson,
    snapshot: &BufferSnapshot,
) -> Result<Range<language::Anchor>, StepErrorKind> {
    let line_count = snapshot.max_point().row + 1;
    if step.start_line < 1 || step.start_line > step.end_line || step.end_line > line_count {
        return Err(StepErrorKind::InvalidRange {
            start_line: step.start_line,
            end_line: step.end_line,
            line_count,
        });
    }
    if step.end_line - step.start_line + 1 > MAX_STEP_LINES {
        return Err(StepErrorKind::RangeTooLong {
            start_line: step.start_line,
            end_line: step.end_line,
        });
    }

    let mut start_row = step.start_line - 1;
    let mut end_row = step.end_line - 1;

    if let Some(anchor_text) = step.anchor_text.as_deref().filter(|text| !text.is_empty()) {
        let line_contains = |row: u32| {
            let line_range = Point::new(row, 0)..Point::new(row, snapshot.line_len(row));
            let line = snapshot.text_for_range(line_range).collect::<String>();
            line.contains(anchor_text)
        };

        if !line_contains(start_row) {
            let not_found = || StepErrorKind::AnchorTextNotFound {
                anchor_text: anchor_text.to_string(),
                start_line: step.start_line,
            };
            let length = end_row - start_row;
            let shifted_start = (1..=ANCHOR_TEXT_SEARCH_RADIUS)
                .flat_map(|distance| {
                    [
                        start_row.checked_sub(distance),
                        start_row.checked_add(distance),
                    ]
                })
                .flatten()
                .filter(|row| *row < line_count)
                .find(|row| line_contains(*row))
                .ok_or_else(not_found)?;
            let shifted_end = shifted_start + length;
            if shifted_end >= line_count {
                return Err(not_found());
            }
            start_row = shifted_start;
            end_row = shifted_end;
        }
    }

    // Biased inwards so that typing at either edge of the range does not
    // grow the highlight, and deleting the code collapses it to empty.
    let start = snapshot.anchor_after(Point::new(start_row, 0));
    let end = snapshot.anchor_before(Point::new(end_row, snapshot.line_len(end_row)));
    Ok(start..end)
}

const FLOWCHART_STATEMENT_KEYWORDS: &[&str] = &[
    "flowchart",
    "graph",
    "subgraph",
    "end",
    "direction",
    "style",
    "classDef",
    "class",
    "linkStyle",
    "click",
    "accTitle",
    "accDescr",
];

/// Extracts the node ids from Mermaid flowchart source, in order of first
/// appearance. Sources that are not flowcharts yield no ids.
pub fn parse_flowchart_node_ids(source: &str) -> Vec<SharedString> {
    let mut node_ids: Vec<SharedString> = Vec::new();
    let mut statements = source
        .lines()
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .filter(|statement| !statement.is_empty() && !statement.starts_with("%%"));

    let is_flowchart = statements.next().is_some_and(|header| {
        let keyword = header.split_whitespace().next().unwrap_or_default();
        keyword == "flowchart" || keyword == "graph"
    });
    if !is_flowchart {
        return node_ids;
    }

    for statement in statements {
        let first_word = statement
            .split(|character: char| !is_node_id_character(character))
            .next()
            .unwrap_or_default();
        let after_first_word = statement[first_word.len()..].chars().next();
        if FLOWCHART_STATEMENT_KEYWORDS.contains(&first_word)
            && after_first_word.is_none_or(char::is_whitespace)
        {
            continue;
        }
        for node_id in statement_node_ids(statement) {
            if !node_ids.iter().any(|existing| existing.as_ref() == node_id) {
                node_ids.push(node_id.to_string().into());
            }
        }
    }
    node_ids
}

fn is_node_id_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_link_character(character: char) -> bool {
    matches!(character, '-' | '=' | '.' | '~' | '<' | '>')
}

fn statement_node_ids(statement: &str) -> Vec<&str> {
    let mut node_ids = Vec::new();
    let mut rest = statement;
    loop {
        rest = rest.trim_start();
        let id_length = rest
            .find(|character: char| !is_node_id_character(character))
            .unwrap_or(rest.len());
        if id_length == 0 {
            break;
        }
        node_ids.push(&rest[..id_length]);
        rest = skip_node_shape(&rest[id_length..]).trim_start();

        if let Some(after_ampersand) = rest.strip_prefix('&') {
            rest = after_ampersand;
            continue;
        }
        let Some(after_link) = skip_link(rest) else {
            break;
        };
        rest = after_link;
    }
    node_ids
}

/// Skips the shape and label that may follow a node id, such as `[Label]`,
/// `{Decision}` or `(("Circle"))`, and a trailing `:::class`.
fn skip_node_shape(text: &str) -> &str {
    let mut depth = 0usize;
    let mut in_quotes = false;
    let mut end = 0;
    for (index, character) in text.char_indices() {
        if in_quotes {
            in_quotes = character != '"';
        } else {
            match character {
                '"' if depth > 0 => in_quotes = true,
                '[' | '(' | '{' => depth += 1,
                '>' if depth == 0 && index == 0 => depth += 1,
                ']' | ')' | '}' if depth > 0 => depth -= 1,
                _ if depth == 0 => break,
                _ => {}
            }
        }
        end = index + character.len_utf8();
        if depth == 0 && !in_quotes {
            break;
        }
    }
    let rest = &text[end..];
    match rest.strip_prefix(":::") {
        Some(class) => {
            let class_length = class
                .find(|character: char| !is_node_id_character(character))
                .unwrap_or(class.len());
            &class[class_length..]
        }
        None => rest,
    }
}

/// Skips a link such as `-->`, `-.->`, `-- text -->` or `-->|text|`,
/// returning what follows it, or `None` if `text` does not start with a link.
fn skip_link(text: &str) -> Option<&str> {
    let run_length = |text: &str| {
        text.find(|character: char| !is_link_character(character))
            .unwrap_or(text.len())
    };
    let skip_end_marker = |text: &'_ str| -> usize {
        let mut characters = text.chars();
        match (characters.next(), characters.next()) {
            (Some('o' | 'x'), Some(next)) if next.is_whitespace() => 1,
            _ => 0,
        }
    };

    let (leading_marker, text) = match text.chars().next() {
        Some('o' | 'x') if text[1..].starts_with("--") || text[1..].starts_with("==") => {
            (true, &text[1..])
        }
        _ => (false, text),
    };
    let opening_length = run_length(text);
    if opening_length < 2 {
        return None;
    }
    let opening = &text[..opening_length];
    let mut rest = &text[opening_length..];
    let marker_length = skip_end_marker(rest);
    rest = &rest[marker_length..];

    if matches!(opening, "--" | "==" | "-.") && !leading_marker && marker_length == 0 {
        let closing_start = link_text_end(rest)?;
        rest = &rest[closing_start..];
        rest = &rest[run_length(rest)..];
        rest = &rest[skip_end_marker(rest)..];
    }

    let trimmed = rest.trim_start();
    if let Some(label) = trimmed.strip_prefix('|') {
        let label_end = label.find('|')?;
        return Some(&label[label_end + 1..]);
    }
    Some(rest)
}

/// Finds where the closing half of a `-- text -->` style link starts.
fn link_text_end(text: &str) -> Option<usize> {
    let mut search_start = 0;
    while let Some(offset) =
        text[search_start..].find(|character: char| is_link_character(character))
    {
        let start = search_start + offset;
        let length = text[start..]
            .find(|character: char| !is_link_character(character))
            .unwrap_or(text.len() - start);
        if length >= 2 {
            return Some(start);
        }
        search_start = start + length;
    }
    None
}

pub struct WalkthroughStore {
    active: Option<Walkthrough>,
    active_step_index: usize,
    next_id: usize,
}

#[derive(Default)]
struct GlobalWalkthroughStores(HashMap<WeakEntity<Project>, Entity<WalkthroughStore>>);

impl Global for GlobalWalkthroughStores {}

impl EventEmitter<WalkthroughEvent> for WalkthroughStore {}

impl WalkthroughStore {
    pub fn for_project(project: &Entity<Project>, cx: &mut App) -> Entity<Self> {
        let weak_project = project.downgrade();
        if let Some(store) = cx
            .default_global::<GlobalWalkthroughStores>()
            .0
            .get(&weak_project)
        {
            return store.clone();
        }

        let store = cx.new(|_| Self {
            active: None,
            active_step_index: 0,
            next_id: 0,
        });
        cx.default_global::<GlobalWalkthroughStores>()
            .0
            .insert(weak_project.clone(), store.clone());
        cx.observe_release(project, move |_, cx| {
            cx.default_global::<GlobalWalkthroughStores>()
                .0
                .remove(&weak_project);
        })
        .detach();
        store
    }

    /// Makes `walkthrough` the active one, replacing any existing
    /// walkthrough, and activates its first step.
    pub fn set_walkthrough(&mut self, mut walkthrough: Walkthrough, cx: &mut Context<Self>) {
        let id = WalkthroughId(self.next_id);
        self.next_id += 1;
        walkthrough.id = id;
        self.active = Some(walkthrough);
        self.active_step_index = 0;
        cx.emit(WalkthroughEvent::Created(id));
        cx.notify();
    }

    pub fn active(&self) -> Option<&Walkthrough> {
        self.active.as_ref()
    }

    pub fn active_step(&self) -> Option<&WalkthroughStep> {
        self.active.as_ref()?.steps.get(self.active_step_index)
    }

    pub fn active_step_index(&self) -> Option<usize> {
        self.active_step().map(|_| self.active_step_index)
    }

    pub fn set_active_step(&mut self, step: &StepId, cx: &mut Context<Self>) {
        let Some(index) = self
            .active
            .as_ref()
            .and_then(|walkthrough| walkthrough.step_index(step))
        else {
            return;
        };
        self.activate_index(index, cx);
    }

    /// Moves `delta` steps forwards or backwards, stopping at either end.
    pub fn step_offset(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(walkthrough) = self.active.as_ref() else {
            return;
        };
        let last_index = walkthrough.steps.len().saturating_sub(1);
        let index = self
            .active_step_index
            .saturating_add_signed(delta)
            .min(last_index);
        self.activate_index(index, cx);
    }

    fn activate_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if index == self.active_step_index {
            return;
        }
        let Some(step) = self
            .active
            .as_ref()
            .and_then(|walkthrough| walkthrough.steps.get(index))
        else {
            return;
        };
        let step = step.id.clone();
        self.active_step_index = index;
        cx.emit(WalkthroughEvent::ActiveStepChanged { step });
        cx.notify();
    }

    pub fn steps_for_buffer(&self, buffer: BufferId) -> impl Iterator<Item = &WalkthroughStep> {
        self.active
            .iter()
            .flat_map(|walkthrough| walkthrough.steps.iter())
            .filter(move |step| step.range.start.buffer_id == buffer)
    }

    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.active.take().is_none() {
            return;
        }
        self.active_step_index = 0;
        cx.emit(WalkthroughEvent::Dismissed);
        cx.notify();
    }
}

/// Wraps a parse check so it can be shared between the agent tool and the
/// fixture loader without this crate depending on the Mermaid renderer.
pub type FlowchartCheck = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// The flowchart parse check registered by the UI layer, if any.
#[derive(Clone)]
pub struct GlobalFlowchartCheck(pub FlowchartCheck);

impl Global for GlobalFlowchartCheck {}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use indoc::indoc;
    use serde_json::json;
    use settings::SettingsStore;
    use std::{cell::RefCell, rc::Rc};
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn numbered_lines(count: usize) -> String {
        (1..=count)
            .map(|line| format!("line {line}\n"))
            .collect::<String>()
    }

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "src": {
                    "one.rs": numbered_lines(100),
                    "two.rs": "fn first() {}\n\nfn second() {\n    body();\n}\n",
                }
            }),
        )
        .await;
        Project::test(fs, [path!("/root").as_ref()], cx).await
    }

    fn step(id: &str, path: &str, start_line: u32, end_line: u32) -> WalkthroughStepJson {
        WalkthroughStepJson {
            id: id.to_string(),
            path: path.to_string(),
            start_line,
            end_line,
            anchor_text: None,
            title: format!("Title of {id}"),
            comment: format!("Comment of {id}"),
            node: None,
        }
    }

    fn walkthrough_json(steps: Vec<WalkthroughStepJson>) -> WalkthroughJson {
        WalkthroughJson {
            title: "Title".to_string(),
            summary: "Summary".to_string(),
            steps,
            flowchart: None,
        }
    }

    async fn load(
        json: WalkthroughJson,
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Result<Walkthrough, Vec<StepError>> {
        cx.update(|cx| Walkthrough::from_json(json, project, |_| Ok(()), cx))
            .await
    }

    async fn load_three_steps(
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Entity<WalkthroughStore> {
        let walkthrough = load(
            walkthrough_json(vec![
                step("a", "root/src/one.rs", 1, 2),
                step("b", "root/src/two.rs", 3, 5),
                step("c", "root/src/one.rs", 10, 12),
            ]),
            project,
            cx,
        )
        .await
        .expect("walkthrough should be valid");
        let store = cx.update(|cx| WalkthroughStore::for_project(project, cx));
        store.update(cx, |store, cx| store.set_walkthrough(walkthrough, cx));
        store
    }

    fn record_events(
        store: &Entity<WalkthroughStore>,
        cx: &mut TestAppContext,
    ) -> Rc<RefCell<Vec<WalkthroughEvent>>> {
        let events = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            cx.subscribe(store, {
                let events = events.clone();
                move |_, event, _| events.borrow_mut().push(event.clone())
            })
            .detach();
        });
        events
    }

    fn error_kinds(errors: Vec<StepError>) -> Vec<(Option<String>, StepErrorKind)> {
        errors
            .into_iter()
            .map(|error| (error.step, error.kind))
            .collect()
    }

    #[gpui::test]
    async fn test_next_and_previous_clamp_and_emit_once(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        let events = record_events(&store, cx);

        let same_store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        assert_eq!(store, same_store);

        let store = load_three_steps(&project, cx).await;
        cx.run_until_parked();
        let id = store.read_with(cx, |store, _| store.active().map(|active| active.id));
        assert_eq!(
            events.take(),
            vec![WalkthroughEvent::Created(id.expect("active walkthrough"))]
        );
        let active_id = |cx: &mut TestAppContext| {
            store.read_with(cx, |store, _| {
                store.active_step().map(|step| step.id.to_string())
            })
        };
        assert_eq!(active_id(cx).as_deref(), Some("a"));

        store.update(cx, |store, cx| store.step_offset(-1, cx));
        cx.run_until_parked();
        assert_eq!(active_id(cx).as_deref(), Some("a"));
        assert_eq!(events.take(), vec![]);

        store.update(cx, |store, cx| store.step_offset(1, cx));
        cx.run_until_parked();
        assert_eq!(active_id(cx).as_deref(), Some("b"));
        assert_eq!(
            events.take(),
            vec![WalkthroughEvent::ActiveStepChanged {
                step: StepId::new("b")
            }]
        );

        store.update(cx, |store, cx| {
            store.step_offset(1, cx);
            store.step_offset(1, cx);
            store.step_offset(1, cx);
        });
        cx.run_until_parked();
        assert_eq!(active_id(cx).as_deref(), Some("c"));
        assert_eq!(
            events.take(),
            vec![WalkthroughEvent::ActiveStepChanged {
                step: StepId::new("c")
            }]
        );

        store.update(cx, |store, cx| {
            store.set_active_step(&StepId::new("c"), cx);
            store.set_active_step(&StepId::new("missing"), cx);
            store.set_active_step(&StepId::new("a"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            events.take(),
            vec![WalkthroughEvent::ActiveStepChanged {
                step: StepId::new("a")
            }]
        );

        store.update(cx, |store, cx| {
            store.dismiss(cx);
            store.dismiss(cx);
            store.step_offset(1, cx);
        });
        cx.run_until_parked();
        assert_eq!(events.take(), vec![WalkthroughEvent::Dismissed]);
        assert!(store.read_with(cx, |store, _| store.active().is_none()));
    }

    #[gpui::test]
    async fn test_steps_for_buffer(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let store = load_three_steps(&project, cx).await;
        store.read_with(cx, |store, cx| {
            let first_buffer = store.active_step().expect("active step").buffer.clone();
            let ids = store
                .steps_for_buffer(first_buffer.read(cx).remote_id())
                .map(|step| step.id.to_string())
                .collect::<Vec<_>>();
            assert_eq!(ids, ["a", "c"]);
            assert_eq!(store.active().map(|active| active.file_count()), Some(2));
        });
    }

    #[gpui::test]
    async fn test_anchor_text_shifts_range(cx: &mut TestAppContext) {
        let project = test_project(cx).await;

        let mut exact = step("exact", "root/src/one.rs", 40, 42);
        exact.anchor_text = Some("line 40".to_string());
        let mut off_by_a_few = step("shifted", "root/src/one.rs", 40, 42);
        off_by_a_few.anchor_text = Some("line 37".to_string());
        let mut later = step("later", "root/src/one.rs", 40, 42);
        later.anchor_text = Some("line 55".to_string());

        let walkthrough = load(
            walkthrough_json(vec![exact, off_by_a_few, later]),
            &project,
            cx,
        )
        .await
        .expect("walkthrough should be valid");
        cx.update(|cx| {
            let rows = walkthrough
                .steps
                .iter()
                .map(|step| {
                    let range = step.point_range(cx);
                    (range.start.row, range.start.column, range.end.row)
                })
                .collect::<Vec<_>>();
            assert_eq!(rows, [(39, 0, 41), (36, 0, 38), (54, 0, 56)]);
            let last = walkthrough.steps[2].point_range(cx);
            assert_eq!(last.end.column, "line 57".len() as u32);
        });

        let mut too_far = step("too-far", "root/src/one.rs", 40, 42);
        too_far.anchor_text = Some("line 70".to_string());
        let errors = load(walkthrough_json(vec![too_far]), &project, cx)
            .await
            .err()
            .expect("anchor text is out of reach");
        assert_eq!(
            error_kinds(errors),
            vec![(
                Some("too-far".to_string()),
                StepErrorKind::AnchorTextNotFound {
                    anchor_text: "line 70".to_string(),
                    start_line: 40,
                }
            )]
        );
    }

    #[gpui::test]
    async fn test_validation_errors_are_collected(cx: &mut TestAppContext) {
        let project = test_project(cx).await;

        let mut unknown_node = step("node", "root/src/one.rs", 1, 2);
        unknown_node.node = Some("nowhere".to_string());
        let mut known_node = step("known", "root/src/one.rs", 3, 4);
        known_node.node = Some("verify".to_string());
        let mut json = walkthrough_json(vec![
            step("missing", "root/src/missing.rs", 1, 2),
            step("directory", "root/src", 1, 1),
            step("out-of-bounds", "root/src/two.rs", 4, 40),
            step("zero", "root/src/two.rs", 0, 1),
            step("backwards", "root/src/two.rs", 3, 2),
            step("too-long", "root/src/one.rs", 1, 61),
            step("dup", "root/src/one.rs", 1, 2),
            step("dup", "root/src/one.rs", 5, 6),
            unknown_node,
            known_node,
        ]);
        json.flowchart = Some("flowchart TD\n  parse --> verify".to_string());

        let errors = load(json.clone(), &project, cx)
            .await
            .err()
            .expect("walkthrough should be invalid");
        let rendered = errors
            .iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            error_kinds(errors),
            vec![
                (Some("dup".to_string()), StepErrorKind::DuplicateId),
                (
                    Some("missing".to_string()),
                    StepErrorKind::PathNotFound {
                        path: "root/src/missing.rs".to_string()
                    }
                ),
                (
                    Some("directory".to_string()),
                    StepErrorKind::PathNotFound {
                        path: "root/src".to_string()
                    }
                ),
                (
                    Some("out-of-bounds".to_string()),
                    StepErrorKind::InvalidRange {
                        start_line: 4,
                        end_line: 40,
                        line_count: 6
                    }
                ),
                (
                    Some("zero".to_string()),
                    StepErrorKind::InvalidRange {
                        start_line: 0,
                        end_line: 1,
                        line_count: 6
                    }
                ),
                (
                    Some("backwards".to_string()),
                    StepErrorKind::InvalidRange {
                        start_line: 3,
                        end_line: 2,
                        line_count: 6
                    }
                ),
                (
                    Some("too-long".to_string()),
                    StepErrorKind::RangeTooLong {
                        start_line: 1,
                        end_line: 61
                    }
                ),
                (
                    Some("node".to_string()),
                    StepErrorKind::UnknownNode {
                        node: "nowhere".to_string()
                    }
                ),
            ]
        );

        json.steps.retain(|step| step.node.is_some());
        let errors = cx
            .update(|cx| {
                Walkthrough::from_json(json, &project, |_| Err("bad syntax".to_string()), cx)
            })
            .await
            .err()
            .expect("flowchart should be rejected");
        assert_eq!(
            error_kinds(errors),
            vec![(
                None,
                StepErrorKind::InvalidFlowchart {
                    message: "bad syntax".to_string()
                }
            )]
        );
        assert_eq!(
            rendered[1],
            "Step `missing`: `root/src/missing.rs` is not a file in the project"
        );

        for count in [0, 31] {
            let steps = (0..count)
                .map(|index| step(&format!("step-{index}"), "root/src/one.rs", 1, 2))
                .collect();
            let errors = load(walkthrough_json(steps), &project, cx)
                .await
                .err()
                .expect("step count is out of bounds");
            assert_eq!(
                error_kinds(errors),
                vec![(None, StepErrorKind::StepCount { count })]
            );
        }

        let mut node_without_flowchart = step("node", "root/src/one.rs", 1, 2);
        node_without_flowchart.node = Some("parse".to_string());
        let errors = load(walkthrough_json(vec![node_without_flowchart]), &project, cx)
            .await
            .err()
            .expect("node needs a flowchart");
        assert_eq!(
            error_kinds(errors),
            vec![(
                Some("node".to_string()),
                StepErrorKind::UnknownNode {
                    node: "parse".to_string()
                }
            )]
        );
    }

    #[gpui::test]
    async fn test_anchors_follow_edits_and_become_stale(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let walkthrough = load(
            walkthrough_json(vec![step("second", "root/src/two.rs", 3, 5)]),
            &project,
            cx,
        )
        .await
        .expect("walkthrough should be valid");
        let step = walkthrough.steps[0].clone();

        cx.update(|cx| {
            assert_eq!(step.point_range(cx), Point::new(2, 0)..Point::new(4, 1));
            assert!(!step.is_stale(cx));
        });

        step.buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, "// added\n// above\n")], None, cx);
        });
        cx.update(|cx| {
            assert_eq!(step.point_range(cx), Point::new(4, 0)..Point::new(6, 1));
            assert!(!step.is_stale(cx));
        });

        let range = cx.update(|cx| step.point_range(cx));
        step.buffer.update(cx, |buffer, cx| {
            buffer.edit([(range, "")], None, cx);
        });
        cx.update(|cx| assert!(step.is_stale(cx)));
    }

    #[test]
    fn test_parse_flowchart_node_ids() {
        let ids = |source: &str| {
            parse_flowchart_node_ids(source)
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids(indoc! {"
                flowchart TD
                  parse[Parse token] --> verify{Valid?}
                  verify -- yes --> handler[Run handler]
                  verify -- no --> reject[401]
            "}),
            ["parse", "verify", "handler", "reject"]
        );
        assert_eq!(
            ids(indoc! {r#"
                %% a comment
                graph LR
                    A["Label with ] and --> inside"] -->|edge label| B((Circle));
                    B -.-> C & D ==> E
                    E -. dotted text .-> F:::highlight
                    F --o G; G x--x H
                    subgraph group [Group title]
                        direction TB
                        I == thick text ==> J_1
                    end
                    style A fill:#f00
                    classDef highlight fill:#0f0
                    class B highlight
                    click A callback
                    endpoint --- K
                    L
            "#}),
            [
                "A", "B", "C", "D", "E", "F", "G", "H", "I", "J_1", "endpoint", "K", "L"
            ]
        );
        assert_eq!(
            ids("sequenceDiagram\n  Alice->>Bob: Hi"),
            Vec::<String>::new()
        );
        assert_eq!(ids(""), Vec::<String>::new());
    }

    #[test]
    fn test_walkthrough_json_matches_tool_input() {
        let json: WalkthroughJson = serde_json::from_value(json!({
            "title": "How request auth works",
            "summary": "A request passes through the auth middleware.",
            "steps": [{
                "id": "parse-token",
                "path": "src/auth/middleware.rs",
                "start_line": 42,
                "end_line": 58,
                "anchor_text": "fn authenticate(",
                "title": "Parse the bearer token",
                "comment": "Markdown explanation.",
                "node": "parse"
            }, {
                "id": "minimal",
                "path": "src/auth/middleware.rs",
                "start_line": 1,
                "end_line": 2,
                "title": "Minimal",
                "comment": "No optional fields."
            }],
            "flowchart": "flowchart TD\n  parse[Parse token] --> verify{Valid?}"
        }))
        .expect("spec example should deserialize");
        assert_eq!(json.steps[0].node.as_deref(), Some("parse"));
        assert_eq!(json.steps[1].anchor_text, None);

        let fixture: WalkthroughJson =
            serde_json::from_str(include_str!("../test_fixtures/sample.json"))
                .expect("fixture should deserialize");
        assert!(fixture.flowchart.is_some());
    }
}
