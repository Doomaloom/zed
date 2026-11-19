mod history;
pub mod mcp_server;

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use collections::{HashMap, HashSet};
use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task,
    WeakEntity, actions,
};
use language::{Buffer, BufferId, BufferSnapshot, Point, ToOffset as _, ToPoint as _};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

pub use history::{HistoryEntry, MAX_WALKTHROUGH_HISTORY, set_history_directory};

pub const MIN_STEPS: usize = 1;
pub const MAX_STEPS: usize = 30;
pub const MAX_STEP_LINES: u32 = 60;
pub const ANCHOR_TEXT_SEARCH_RADIUS: u32 = 20;
pub const MAX_STEP_HIGHLIGHTS: usize = 8;
pub const MAX_STEP_DATA: usize = 8;
pub const MAX_DATA_VALUE_LENGTH: usize = 400;

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
        /// Shows a code walkthrough that was shown before in this project.
        Reopen,
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
    pub legend: Vec<LegendEntry>,
    pub example: Option<SharedString>,
}

impl Walkthrough {
    pub fn step_index(&self, step: &StepId) -> Option<usize> {
        self.steps
            .iter()
            .position(|candidate| &candidate.id == step)
    }

    pub fn legend_label(&self, color: HighlightColor) -> Option<&SharedString> {
        self.legend
            .iter()
            .find(|entry| entry.color == color)
            .map(|entry| &entry.label)
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
    pub color: Option<HighlightColor>,
    pub highlights: Vec<StepHighlight>,
    pub data: Vec<StepData>,
    /// How to show the step's file: its name for files in the project, the
    /// absolute path for files outside it, whose worktree has no useful name.
    pub file_label: SharedString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepData {
    pub name: SharedString,
    pub value: SharedString,
    pub change: DataChange,
}

/// How a piece of example data compares with the most recent earlier step
/// that listed the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataChange {
    New,
    Changed { previous: SharedString },
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepHighlight {
    pub range: Range<language::Anchor>,
    pub color: HighlightColor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegendEntry {
    pub color: HighlightColor,
    pub label: SharedString,
}

impl WalkthroughStep {
    /// The colors this step uses, in the order they first appear.
    pub fn colors(&self) -> Vec<HighlightColor> {
        let mut colors = Vec::new();
        for color in self
            .color
            .into_iter()
            .chain(self.highlights.iter().map(|highlight| highlight.color))
        {
            if !colors.contains(&color) {
                colors.push(color);
            }
        }
        colors
    }

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
    /// The active walkthrough was replaced by an edited version of itself.
    /// `active_step_moved` is set when the step the user is on is a different
    /// step, or covers different code, than before.
    Updated {
        id: WalkthroughId,
        active_step_moved: bool,
        flowchart_added: bool,
    },
    ActiveStepChanged {
        step: StepId,
    },
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
/// - Color is optional. Use it only when it carries meaning, such as input versus mutation versus error path, or one color per subsystem. Keep one meaning per color across the whole walkthrough and use at most 4 colors.
/// - Every color you use must have an entry in `legend` with a label of 1 to 4 words. The legend is shown to the user.
/// - Use a step's `highlights` to mark the exact expression or lines a comment talks about, and refer to them in the comment by their legend label.
/// - When the code transforms data, set `example` to one or two sentences naming a single concrete scenario, and give each step `data` holding the few values in play at that step. Keep values short, list only values relevant to the step, and reuse the same `name` across steps so the user sees how a value changes.
/// - To change or extend a walkthrough that is already showing, use `edit_walkthrough` (and `get_walkthrough` to see it) instead of creating a new one.
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
    /// What each color means in this walkthrough. One entry for every color
    /// used by a step or a highlight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legend: Vec<LegendEntryJson>,
    /// One or two sentences naming the example scenario that the steps' `data`
    /// belongs to, such as "A signed-in user adds 2 units of SKU A-12 to an
    /// empty cart".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
}

/// A highlight color. The meaning of each color is given by the walkthrough's `legend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HighlightColor {
    Blue,
    Green,
    Yellow,
    Orange,
    Red,
    Purple,
}

impl HighlightColor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Orange => "orange",
            Self::Red => "red",
            Self::Purple => "purple",
        }
    }
}

impl std::fmt::Display for HighlightColor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LegendEntryJson {
    pub color: HighlightColor,
    /// What this color means in this walkthrough, in 1 to 4 words.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StepHighlightJson {
    /// The first line of the highlight (1-based, inclusive). Must lie within the step's range.
    pub start_line: u32,
    /// The last line of the highlight (1-based, inclusive). Must lie within the step's range.
    pub end_line: u32,
    /// When given, only the first occurrence of this exact text within the
    /// lines is highlighted. Otherwise the whole lines are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub color: HighlightColor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StepDataJson {
    /// The name of the value, such as a variable or field. Reuse the same name
    /// in later steps to show how the value changes. Unique within the step.
    pub name: String,
    /// The value in this example scenario, as short text or a JSON literal.
    /// At most 400 characters.
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalkthroughStepJson {
    /// A short slug identifying this step, unique within the walkthrough.
    pub id: String,
    /// The path of the file to annotate, relative to the project root or
    /// starting with the project's root directory name. It may instead be an
    /// absolute path to a file in another project on the same machine, so that
    /// one walkthrough can span, for example, a frontend and a backend.
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
    /// The color of this step's range. Steps without one use the default highlight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<HighlightColor>,
    /// Smaller colored highlights inside this step's range. At most 8.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlights: Vec<StepHighlightJson>,
    /// The values in play at this step in the walkthrough's `example`
    /// scenario. At most 8.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<StepDataJson>,
}

/// Returns the code walkthrough that is currently showing in this project, in
/// the same format `create_walkthrough` accepts, and says which step the user
/// is on. Call this before `edit_walkthrough` so the edit is based on what the
/// user sees.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GetWalkthroughJson {}

/// Changes the code walkthrough that is already showing, in place. The user
/// stays on their current step. Use it when the user asks to explain a step in
/// more detail, add or remove steps, or fix the flowchart, instead of calling
/// `create_walkthrough` again. Call `get_walkthrough` first to see the current
/// step ids and content.
///
/// - Only include what changes. Every field is optional, but at least one must be given.
/// - A step in `steps` is a complete step, with the same fields as in `create_walkthrough`. When its `id` already exists, it replaces that step in place, so repeat every field you want to keep.
/// - A step whose `id` is new is inserted after the step named by `after`, or appended at the end when `after` is not given. Steps are applied in order, so a later new step may use an earlier new step's id as `after`.
/// - If you change `flowchart`, `legend` or step `node` and `color` values, check the result is still consistent: every `node` must exist in the flowchart and every color used must be in the legend.
/// - The result is validated like a new walkthrough. If anything is wrong, nothing is changed and every problem is listed.
/// - After the walkthrough is updated, reply with one or two sentences. Do not repeat the explanation in chat.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalkthroughEditJson {
    /// A new title for the walkthrough. Leave out to keep the current one.
    #[serde(default)]
    pub title: Option<String>,
    /// A new markdown summary. Leave out to keep the current one.
    #[serde(default)]
    pub summary: Option<String>,
    /// New Mermaid source for the flowchart. Leave out to keep the current
    /// one. A flowchart cannot be removed with this tool.
    #[serde(default)]
    pub flowchart: Option<String>,
    /// New example scenario text. Leave out to keep the current one. An
    /// example cannot be removed with this tool.
    #[serde(default)]
    pub example: Option<String>,
    /// The complete new legend, replacing the current one. Leave out to keep
    /// the current legend. An empty list removes the legend.
    #[serde(default)]
    pub legend: Option<Vec<LegendEntryJson>>,
    /// Steps to replace or insert, applied in order after `remove_steps`.
    #[serde(default)]
    pub steps: Vec<WalkthroughStepEditJson>,
    /// Ids of existing steps to delete.
    #[serde(default)]
    pub remove_steps: Vec<String>,
}

impl WalkthroughEditJson {
    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.summary.is_none()
            && self.flowchart.is_none()
            && self.example.is_none()
            && self.legend.is_none()
            && self.steps.is_empty()
            && self.remove_steps.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalkthroughStepEditJson {
    #[serde(flatten)]
    pub step: WalkthroughStepJson,
    /// The id of the existing step to insert this step after. Only for a step
    /// whose `id` is new: leave it out to append the step at the end, and
    /// leave it out when replacing an existing step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
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
    TooManyHighlights {
        count: usize,
    },
    HighlightOutsideStep {
        start_line: u32,
        end_line: u32,
    },
    HighlightTextNotFound {
        text: String,
        start_line: u32,
        end_line: u32,
    },
    DuplicateLegendColor {
        color: HighlightColor,
    },
    ColorWithoutLegend {
        color: HighlightColor,
    },
    TooManyData {
        count: usize,
    },
    EmptyDataName,
    DuplicateDataName {
        name: String,
    },
    DataValueTooLong {
        name: String,
        length: usize,
    },
    UnknownRemovedStep {
        id: String,
    },
    UnknownAfterStep {
        after: String,
    },
    AfterOnExistingStep,
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
            StepErrorKind::TooManyHighlights { count } => write!(
                formatter,
                "a step can have at most {MAX_STEP_HIGHLIGHTS} highlights, but {count} were given"
            ),
            StepErrorKind::HighlightOutsideStep {
                start_line,
                end_line,
            } => write!(
                formatter,
                "highlight lines {start_line}-{end_line} are not a valid range within the step's lines"
            ),
            StepErrorKind::HighlightTextNotFound {
                text,
                start_line,
                end_line,
            } => write!(
                formatter,
                "highlight text `{text}` was not found in lines {start_line}-{end_line}"
            ),
            StepErrorKind::DuplicateLegendColor { color } => {
                write!(formatter, "the legend lists `{color}` more than once")
            }
            StepErrorKind::ColorWithoutLegend { color } => write!(
                formatter,
                "the color `{color}` is used but has no entry in `legend`"
            ),
            StepErrorKind::TooManyData { count } => write!(
                formatter,
                "a step can have at most {MAX_STEP_DATA} data entries, but {count} were given"
            ),
            StepErrorKind::EmptyDataName => {
                write!(formatter, "a data entry has an empty `name`")
            }
            StepErrorKind::DuplicateDataName { name } => {
                write!(
                    formatter,
                    "the data name `{name}` is used more than once in this step"
                )
            }
            StepErrorKind::DataValueTooLong { name, length } => write!(
                formatter,
                "the value of data `{name}` is {length} characters, but at most {MAX_DATA_VALUE_LENGTH} are allowed"
            ),
            StepErrorKind::UnknownRemovedStep { id } => write!(
                formatter,
                "`remove_steps` lists `{id}`, which is not a step id in the walkthrough"
            ),
            StepErrorKind::UnknownAfterStep { after } => write!(
                formatter,
                "`after` is `{after}`, which is not a step id in the walkthrough"
            ),
            StepErrorKind::AfterOnExistingStep => write!(
                formatter,
                "this id already exists, so the step is replaced in place; leave out `after`"
            ),
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

        let mut legend_colors = HashSet::default();
        let mut reported_colors = HashSet::default();
        for entry in &json.legend {
            if !legend_colors.insert(entry.color) && reported_colors.insert(entry.color) {
                errors.push(StepError {
                    step: None,
                    kind: StepErrorKind::DuplicateLegendColor { color: entry.color },
                });
            }
        }
        let used_colors = json.steps.iter().flat_map(|step| {
            step.color
                .into_iter()
                .chain(step.highlights.iter().map(|highlight| highlight.color))
        });
        for color in used_colors {
            if !legend_colors.contains(&color) && reported_colors.insert(color) {
                errors.push(StepError {
                    step: None,
                    kind: StepErrorKind::ColorWithoutLegend { color },
                });
            }
        }

        for step in &json.steps {
            errors.extend(
                validate_step_data(&step.data)
                    .into_iter()
                    .map(|kind| StepError {
                        step: Some(step.id.clone()),
                        kind,
                    }),
            );
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
                if let Some(project_path) = project_path {
                    let open_buffer = project.update(cx, |project, cx| {
                        project.open_buffer(project_path.clone(), cx)
                    });
                    return StepFile::InProject(project_path, open_buffer);
                }
                let is_absolute = project
                    .read(cx)
                    .path_style(cx)
                    .has_root(Path::new(&step.path));
                if is_absolute {
                    StepFile::OutsideProject(open_outside_project_file(
                        step.path.clone(),
                        project.clone(),
                        cx,
                    ))
                } else {
                    StepFile::Missing
                }
            })
            .collect::<Vec<_>>();

        cx.spawn(async move |cx| {
            let mut steps = Vec::new();
            let mut latest_values = HashMap::default();
            for (step_json, step_file) in json.steps.iter().zip(open_buffers) {
                let (project_path, buffer, file_label) = match step_file {
                    StepFile::Missing => {
                        errors.push(path_not_found(step_json));
                        continue;
                    }
                    StepFile::InProject(project_path, open_buffer) => {
                        let file_label = project_path
                            .path
                            .file_name()
                            .map(|name| name.to_string())
                            .unwrap_or_default();
                        match open_buffer.await {
                            Ok(buffer) => (project_path, buffer, file_label),
                            Err(error) => {
                                errors.push(open_failed(step_json, &error));
                                continue;
                            }
                        }
                    }
                    StepFile::OutsideProject(open_file) => match open_file.await {
                        Ok(Some((project_path, buffer))) => {
                            (project_path, buffer, step_json.path.clone())
                        }
                        Ok(None) => {
                            errors.push(path_not_found(step_json));
                            continue;
                        }
                        Err(error) => {
                            errors.push(open_failed(step_json, &error));
                            continue;
                        }
                    },
                };
                let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
                let rows = match resolve_step_rows(step_json, &snapshot) {
                    Ok(rows) => rows,
                    Err(kind) => {
                        errors.push(StepError {
                            step: Some(step_json.id.clone()),
                            kind,
                        });
                        continue;
                    }
                };
                let (highlights, highlight_errors) =
                    resolve_step_highlights(step_json, rows.start, &snapshot);
                errors.extend(highlight_errors.into_iter().map(|kind| StepError {
                    step: Some(step_json.id.clone()),
                    kind,
                }));
                // Biased inwards so that typing at either edge of the range does not
                // grow the highlight, and deleting the code collapses it to empty.
                let start = snapshot.anchor_after(Point::new(rows.start, 0));
                let end = snapshot.anchor_before(Point::new(rows.end, snapshot.line_len(rows.end)));
                steps.push(WalkthroughStep {
                    id: StepId::new(step_json.id.clone()),
                    path: project_path,
                    buffer,
                    range: start..end,
                    title: step_json.title.clone().into(),
                    comment: step_json.comment.clone().into(),
                    node_id: step_json.node.clone().map(SharedString::from),
                    color: step_json.color,
                    highlights,
                    data: resolve_step_data(&step_json.data, &mut latest_values),
                    file_label: file_label.into(),
                });
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
                    legend: json
                        .legend
                        .into_iter()
                        .map(|entry| LegendEntry {
                            color: entry.color,
                            label: entry.label.into(),
                        })
                        .collect(),
                    example: json.example.map(SharedString::from),
                })
            } else {
                Err(errors)
            }
        })
    }
}

enum StepFile {
    Missing,
    InProject(ProjectPath, Task<anyhow::Result<Entity<Buffer>>>),
    OutsideProject(Task<anyhow::Result<Option<(ProjectPath, Entity<Buffer>)>>>),
}

fn path_not_found(step: &WalkthroughStepJson) -> StepError {
    StepError {
        step: Some(step.id.clone()),
        kind: StepErrorKind::PathNotFound {
            path: step.path.clone(),
        },
    }
}

fn open_failed(step: &WalkthroughStepJson, error: &anyhow::Error) -> StepError {
    StepError {
        step: Some(step.id.clone()),
        kind: StepErrorKind::OpenFailed {
            path: step.path.clone(),
            message: error.to_string(),
        },
    }
}

/// Opens a file that is not in any visible worktree by adding it to the
/// project as a non-visible worktree, as opening an arbitrary file from disk
/// does. The buffer keeps the worktree alive. Resolves to `None` when the path
/// is not a file.
fn open_outside_project_file(
    absolute_path: String,
    project: Entity<Project>,
    cx: &mut App,
) -> Task<anyhow::Result<Option<(ProjectPath, Entity<Buffer>)>>> {
    let find_worktree = project.update(cx, |project, cx| {
        project.find_or_create_worktree(&absolute_path, false, cx)
    });
    cx.spawn(async move |cx| {
        let (worktree, relative_path) = find_worktree.await?;
        // A worktree for a path that does not exist is created without
        // entries, so the file's presence is only known after the scan.
        let scan_complete = worktree.read_with(cx, |worktree, _| {
            worktree.as_local().map(|local| local.scan_complete())
        });
        if let Some(scan_complete) = scan_complete {
            scan_complete.await;
        }
        let project_path = worktree.read_with(cx, |worktree, _| ProjectPath {
            worktree_id: worktree.id(),
            path: relative_path,
        });
        let is_file = project.read_with(cx, |project, cx| {
            project
                .entry_for_path(&project_path, cx)
                .is_some_and(|entry| entry.is_file())
        });
        if !is_file {
            return Ok(None);
        }
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer(project_path.clone(), cx)
            })
            .await?;
        Ok(Some((project_path, buffer)))
    })
}

fn validate_step_data(data: &[StepDataJson]) -> Vec<StepErrorKind> {
    let mut errors = Vec::new();
    if data.len() > MAX_STEP_DATA {
        errors.push(StepErrorKind::TooManyData { count: data.len() });
        return errors;
    }
    let mut seen_names = HashSet::default();
    let mut reported_names = HashSet::default();
    for entry in data {
        if entry.name.trim().is_empty() {
            errors.push(StepErrorKind::EmptyDataName);
        } else if !seen_names.insert(entry.name.as_str())
            && reported_names.insert(entry.name.as_str())
        {
            errors.push(StepErrorKind::DuplicateDataName {
                name: entry.name.clone(),
            });
        }
        let length = entry.value.chars().count();
        if length > MAX_DATA_VALUE_LENGTH {
            errors.push(StepErrorKind::DataValueTooLong {
                name: entry.name.clone(),
                length,
            });
        }
    }
    errors
}

/// Compares each entry with the most recent earlier step that had the same
/// name, which `latest_values` tracks across calls in step order.
fn resolve_step_data(
    data: &[StepDataJson],
    latest_values: &mut HashMap<String, SharedString>,
) -> Vec<StepData> {
    data.iter()
        .map(|entry| {
            let value = SharedString::from(entry.value.clone());
            let change = match latest_values.insert(entry.name.clone(), value.clone()) {
                None => DataChange::New,
                Some(previous) if previous != value => DataChange::Changed { previous },
                Some(_) => DataChange::Unchanged,
            };
            StepData {
                name: entry.name.clone().into(),
                value,
                change,
            }
        })
        .collect()
}

/// Resolves the first and last row of a step, both inclusive.
fn resolve_step_rows(
    step: &WalkthroughStepJson,
    snapshot: &BufferSnapshot,
) -> Result<Range<u32>, StepErrorKind> {
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

    Ok(start_row..end_row)
}

/// Resolves a step's inner highlights. `step_start_row` is the row the step
/// resolved to, which differs from `start_line` when `anchor_text` moved the
/// step; the highlights move with it.
fn resolve_step_highlights(
    step: &WalkthroughStepJson,
    step_start_row: u32,
    snapshot: &BufferSnapshot,
) -> (Vec<StepHighlight>, Vec<StepErrorKind>) {
    let mut highlights = Vec::new();
    let mut errors = Vec::new();
    if step.highlights.len() > MAX_STEP_HIGHLIGHTS {
        errors.push(StepErrorKind::TooManyHighlights {
            count: step.highlights.len(),
        });
        return (highlights, errors);
    }

    for highlight in &step.highlights {
        if highlight.start_line < step.start_line
            || highlight.end_line > step.end_line
            || highlight.start_line > highlight.end_line
        {
            errors.push(StepErrorKind::HighlightOutsideStep {
                start_line: highlight.start_line,
                end_line: highlight.end_line,
            });
            continue;
        }
        let start_row = step_start_row + (highlight.start_line - step.start_line);
        let end_row = step_start_row + (highlight.end_line - step.start_line);
        let lines = Point::new(start_row, 0)..Point::new(end_row, snapshot.line_len(end_row));

        let offsets = match highlight.text.as_deref().filter(|text| !text.is_empty()) {
            Some(text) => {
                let lines_start = lines.start.to_offset(snapshot);
                let haystack = snapshot.text_for_range(lines).collect::<String>();
                let Some(found) = haystack.find(text) else {
                    errors.push(StepErrorKind::HighlightTextNotFound {
                        text: text.to_string(),
                        start_line: highlight.start_line,
                        end_line: highlight.end_line,
                    });
                    continue;
                };
                lines_start + found..lines_start + found + text.len()
            }
            None => lines.start.to_offset(snapshot)..lines.end.to_offset(snapshot),
        };
        highlights.push(StepHighlight {
            range: snapshot.anchor_after(offsets.start)..snapshot.anchor_before(offsets.end),
            color: highlight.color,
        });
    }
    (highlights, errors)
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
    /// The input the active walkthrough was built from.
    active_json: Option<WalkthroughJson>,
    active_step_index: usize,
    next_id: usize,
    project: WeakEntity<Project>,
    history: Vec<HistoryEntry>,
    history_loaded: bool,
    history_path: Option<PathBuf>,
    save_history_task: Option<Task<()>>,
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
            active_json: None,
            active_step_index: 0,
            next_id: 0,
            project: weak_project.clone(),
            history: Vec::new(),
            history_loaded: false,
            history_path: None,
            save_history_task: None,
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
    pub fn set_walkthrough(&mut self, walkthrough: Walkthrough, cx: &mut Context<Self>) {
        self.install_walkthrough(walkthrough, None, cx);
    }

    fn install_walkthrough(
        &mut self,
        mut walkthrough: Walkthrough,
        source: Option<WalkthroughJson>,
        cx: &mut Context<Self>,
    ) {
        let id = WalkthroughId(self.next_id);
        self.next_id += 1;
        walkthrough.id = id;
        self.active = Some(walkthrough);
        self.active_json = source;
        self.active_step_index = 0;
        cx.emit(WalkthroughEvent::Created(id));
        cx.notify();
    }

    /// Swaps the active walkthrough for `walkthrough`, an edited version of
    /// the one built from `base`, keeping the user on the same step where it
    /// still exists. Returns `false` and changes nothing when the active
    /// walkthrough is no longer the one `base` describes.
    fn replace_walkthrough(
        &mut self,
        mut walkthrough: Walkthrough,
        source: WalkthroughJson,
        base: &WalkthroughJson,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.active_json.as_ref() != Some(base) {
            return false;
        }
        let Some(previous) = self.active.as_ref() else {
            return false;
        };
        let previous_step = previous.steps.get(self.active_step_index);
        let last_index = walkthrough.steps.len().saturating_sub(1);
        let index = previous_step
            .and_then(|step| walkthrough.step_index(&step.id))
            .unwrap_or_else(|| self.active_step_index.min(last_index));
        let active_step_moved = match (previous_step, walkthrough.steps.get(index)) {
            (Some(previous_step), Some(step)) => {
                previous_step.id != step.id
                    || previous_step.path != step.path
                    || previous_step.point_range(cx) != step.point_range(cx)
            }
            _ => true,
        };
        let flowchart_added = previous.flowchart.is_none() && walkthrough.flowchart.is_some();

        let id = previous.id;
        walkthrough.id = id;
        self.active = Some(walkthrough);
        self.active_json = Some(source);
        self.active_step_index = index;
        cx.emit(WalkthroughEvent::Updated {
            id,
            active_step_moved,
            flowchart_added,
        });
        cx.notify();
        true
    }

    pub fn active(&self) -> Option<&Walkthrough> {
        self.active.as_ref()
    }

    /// The input the active walkthrough was built from, in the format agents
    /// create it in. `None` when there is no active walkthrough.
    pub fn active_json(&self) -> Option<&WalkthroughJson> {
        self.active_json.as_ref()
    }

    /// The walkthroughs shown for this project, most recent first. Empty
    /// until the task from [`Self::load_history`] has finished.
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// Reads the project's saved history the first time it is needed. Projects
    /// without a stable local file keep their history in memory only.
    pub fn load_history(&mut self, cx: &mut Context<Self>) -> Task<()> {
        if self.history_loaded {
            return Task::ready(());
        }
        let Some(project) = self.project.upgrade() else {
            self.history_loaded = true;
            return Task::ready(());
        };
        let Some(path) = history::history_file(project.read(cx), cx) else {
            self.history_loaded = true;
            return Task::ready(());
        };
        let fs = project.read(cx).fs().clone();
        cx.spawn(async move |this, cx| {
            let entries = cx
                .background_spawn({
                    let path = path.clone();
                    async move { history::load_entries(fs.as_ref(), &path).await }
                })
                .await
                .log_err()
                .unwrap_or_default();
            this.update(cx, |store, cx| {
                if store.history_loaded {
                    return;
                }
                store.history_loaded = true;
                store.history_path = Some(path);
                store.history = entries;
                cx.notify();
            })
            .log_err();
        })
    }

    /// Moves `walkthrough` to the front of the history, or adds it there, and
    /// saves the history. An entry equal to `replaces`, the walkthrough that
    /// `walkthrough` is an edit of, is dropped. [`Self::load_history`] must
    /// have finished first, or the saved file would lose its older entries.
    fn record_history(
        &mut self,
        walkthrough: WalkthroughJson,
        replaces: Option<&WalkthroughJson>,
        cx: &mut Context<Self>,
    ) {
        self.history.retain(|entry| {
            entry.walkthrough != walkthrough && Some(&entry.walkthrough) != replaces
        });
        self.history.insert(0, HistoryEntry::now(walkthrough));
        self.history.truncate(MAX_WALKTHROUGH_HISTORY);
        cx.notify();

        let Some(path) = self.history_path.clone() else {
            return;
        };
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let fs = project.read(cx).fs().clone();
        let entries = self.history.clone();
        self.save_history_task = Some(cx.background_spawn(async move {
            history::save_entries(fs.as_ref(), &path, &entries)
                .await
                .log_err();
        }));
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
        self.active_json = None;
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

/// What was shown by a successful [`activate_walkthrough`].
pub struct ShownWalkthrough {
    pub step_count: usize,
    pub file_count: usize,
}

fn validate_walkthrough(
    json: WalkthroughJson,
    project: &Entity<Project>,
    cx: &mut App,
) -> Task<Result<Walkthrough, Vec<StepError>>> {
    // Without a UI layer there is no renderer to validate against, and
    // nothing that would later fail to draw the flowchart.
    let flowchart_check = cx
        .try_global::<GlobalFlowchartCheck>()
        .map(|check| check.0.clone());
    Walkthrough::from_json(
        json,
        project,
        move |source| match flowchart_check {
            Some(check) => check(source),
            None => Ok(()),
        },
        cx,
    )
}

/// Validates `json` against `project` and, when it is valid, makes it the
/// project's active walkthrough and records it in the project's history. This
/// is the one place every walkthrough passes through before it is shown.
pub fn activate_walkthrough(
    json: WalkthroughJson,
    project: &Entity<Project>,
    cx: &mut App,
) -> Task<Result<ShownWalkthrough, Vec<StepError>>> {
    let project = project.clone();
    let load = validate_walkthrough(json.clone(), &project, cx);

    cx.spawn(async move |cx| {
        let walkthrough = load.await?;
        let shown = ShownWalkthrough {
            step_count: walkthrough.steps.len(),
            file_count: walkthrough.file_count(),
        };
        let store = cx.update(|cx| {
            let store = WalkthroughStore::for_project(&project, cx);
            store.update(cx, |store, cx| {
                store.install_walkthrough(walkthrough, Some(json.clone()), cx)
            });
            store
        });

        let history_loaded = store.update(cx, |store, cx| store.load_history(cx));
        history_loaded.await;
        store.update(cx, |store, cx| store.record_history(json, None, cx));
        Ok(shown)
    })
}

fn problems_message(outcome: &str, errors: Vec<StepError>) -> String {
    let mut message = format!(
        "The walkthrough was not {outcome}. Fix {} and call the tool again:",
        if errors.len() == 1 {
            "this problem".to_string()
        } else {
            format!("these {} problems", errors.len())
        }
    );
    for step_error in errors {
        message.push_str(&format!("\n- {step_error}"));
    }
    message
}

fn counts_message(step_count: usize, file_count: usize) -> String {
    format!(
        "{step_count} {} across {file_count} {}",
        if step_count == 1 { "step" } else { "steps" },
        if file_count == 1 { "file" } else { "files" },
    )
}

/// Validates `json` against `project` and, when it is valid, makes it the
/// project's active walkthrough. Returns the message to give the calling
/// agent: a confirmation on success, or every problem found on failure.
pub fn show_walkthrough(
    json: WalkthroughJson,
    project: &Entity<Project>,
    cx: &mut App,
) -> Task<Result<String, String>> {
    let activate = activate_walkthrough(json, project, cx);
    cx.spawn(async move |_| {
        let shown = activate
            .await
            .map_err(|errors| problems_message("shown", errors))?;
        Ok(format!(
            "Walkthrough shown to the user: {}.",
            counts_message(shown.step_count, shown.file_count)
        ))
    })
}

const NO_ACTIVE_WALKTHROUGH_MESSAGE: &str = "No walkthrough is active in this project.";

/// Describes the project's active walkthrough for an agent: the step the user
/// is on, then the walkthrough in the format `create_walkthrough` accepts.
pub fn describe_walkthrough(project: &Entity<Project>, cx: &mut App) -> String {
    let store = WalkthroughStore::for_project(project, cx);
    let store = store.read(cx);
    let (Some(walkthrough), Some(json), Some(step)) =
        (store.active(), store.active_json(), store.active_step())
    else {
        return NO_ACTIVE_WALKTHROUGH_MESSAGE.to_string();
    };
    match serde_json::to_string_pretty(json) {
        Ok(text) => format!(
            "The user is on step \"{}\" ({} of {}).\n{text}",
            step.id,
            store.active_step_index + 1,
            walkthrough.steps.len(),
        ),
        Err(error) => format!("The walkthrough could not be written out: {error}"),
    }
}

fn apply_edit(
    base: &WalkthroughJson,
    edit: WalkthroughEditJson,
) -> (WalkthroughJson, Vec<StepError>) {
    let mut merged = base.clone();
    let mut errors = Vec::new();
    if let Some(title) = edit.title {
        merged.title = title;
    }
    if let Some(summary) = edit.summary {
        merged.summary = summary;
    }
    if let Some(flowchart) = edit.flowchart {
        merged.flowchart = Some(flowchart);
    }
    if let Some(example) = edit.example {
        merged.example = Some(example);
    }
    if let Some(legend) = edit.legend {
        merged.legend = legend;
    }

    for id in edit.remove_steps {
        match merged.steps.iter().position(|step| step.id == id) {
            Some(index) => {
                merged.steps.remove(index);
            }
            None => errors.push(StepError {
                step: None,
                kind: StepErrorKind::UnknownRemovedStep { id },
            }),
        }
    }

    for WalkthroughStepEditJson { step, after } in edit.steps {
        let existing = merged
            .steps
            .iter()
            .position(|candidate| candidate.id == step.id);
        match (existing, after) {
            (Some(index), None) => {
                if let Some(slot) = merged.steps.get_mut(index) {
                    *slot = step;
                }
            }
            (Some(_), Some(_)) => errors.push(StepError {
                step: Some(step.id),
                kind: StepErrorKind::AfterOnExistingStep,
            }),
            (None, None) => merged.steps.push(step),
            (None, Some(after)) => {
                match merged
                    .steps
                    .iter()
                    .position(|candidate| candidate.id == after)
                {
                    Some(index) => merged.steps.insert(index + 1, step),
                    None => errors.push(StepError {
                        step: Some(step.id),
                        kind: StepErrorKind::UnknownAfterStep { after },
                    }),
                }
            }
        }
    }
    (merged, errors)
}

/// Applies `edit` to the project's active walkthrough in place. The edited
/// walkthrough goes through the same validation as a new one, and on any
/// problem nothing changes. Returns the message to give the calling agent.
pub fn edit_walkthrough(
    edit: WalkthroughEditJson,
    project: &Entity<Project>,
    cx: &mut App,
) -> Task<Result<String, String>> {
    let store = WalkthroughStore::for_project(project, cx);
    let Some(base) = store.read(cx).active_json().cloned() else {
        return Task::ready(Err(
            "No walkthrough is active. Use create_walkthrough first.".to_string(),
        ));
    };
    if edit.is_empty() {
        return Task::ready(Err(
            "The walkthrough was not changed: the edit sets no field. Give at least one of title, summary, flowchart, example, legend, steps or remove_steps.".to_string(),
        ));
    }

    let (merged, edit_errors) = apply_edit(&base, edit);
    let validate = validate_walkthrough(merged.clone(), project, cx);
    let project = project.clone();
    cx.spawn(async move |cx| {
        let walkthrough = match validate.await {
            Ok(walkthrough) if edit_errors.is_empty() => walkthrough,
            outcome => {
                let mut errors = edit_errors;
                if let Err(validation_errors) = outcome {
                    errors.extend(validation_errors);
                }
                return Err(problems_message("changed", errors));
            }
        };
        let step_ids = walkthrough
            .steps
            .iter()
            .map(|step| step.id.to_string())
            .collect::<Vec<_>>();
        let counts = counts_message(walkthrough.steps.len(), walkthrough.file_count());

        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        let replaced = store.update(cx, |store, cx| {
            store.replace_walkthrough(walkthrough, merged.clone(), &base, cx)
        });
        if !replaced {
            return Err("The walkthrough was not changed: it was replaced or dismissed while this edit was being checked. Call get_walkthrough and try again.".to_string());
        }

        let history_loaded = store.update(cx, |store, cx| store.load_history(cx));
        history_loaded.await;
        store.update(cx, |store, cx| {
            store.record_history(merged, Some(&base), cx)
        });
        Ok(format!(
            "Walkthrough updated: {counts}. Step order: {}",
            step_ids.join(", ")
        ))
    })
}

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

    struct HistoryDirectoryGuard(#[allow(dead_code)] tempfile::TempDir);

    impl Global for HistoryDirectoryGuard {}

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            let directory = tempfile::tempdir().expect("creating a history directory");
            set_history_directory(directory.path().to_path_buf(), cx);
            cx.set_global(HistoryDirectoryGuard(directory));
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
            color: None,
            highlights: Vec::new(),
            data: Vec::new(),
        }
    }

    fn data(name: &str, value: &str) -> StepDataJson {
        StepDataJson {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    fn walkthrough_json(steps: Vec<WalkthroughStepJson>) -> WalkthroughJson {
        WalkthroughJson {
            title: "Title".to_string(),
            summary: "Summary".to_string(),
            steps,
            flowchart: None,
            legend: Vec::new(),
            example: None,
        }
    }

    fn highlight(
        start_line: u32,
        end_line: u32,
        text: Option<&str>,
        color: HighlightColor,
    ) -> StepHighlightJson {
        StepHighlightJson {
            start_line,
            end_line,
            text: text.map(str::to_string),
            color,
        }
    }

    fn legend(colors: &[HighlightColor]) -> Vec<LegendEntryJson> {
        colors
            .iter()
            .map(|color| LegendEntryJson {
                color: *color,
                label: format!("Meaning of {color}"),
            })
            .collect()
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
    async fn test_colors_and_highlights_resolve(cx: &mut TestAppContext) {
        let project = test_project(cx).await;

        let mut plain = step("plain", "root/src/two.rs", 3, 5);
        plain.color = Some(HighlightColor::Blue);
        plain.highlights = vec![
            highlight(4, 4, None, HighlightColor::Red),
            highlight(3, 5, Some("body()"), HighlightColor::Green),
        ];
        let mut shifted = step("shifted", "root/src/one.rs", 40, 42);
        shifted.anchor_text = Some("line 37".to_string());
        shifted.highlights = vec![highlight(41, 42, Some("38"), HighlightColor::Red)];

        let mut json = walkthrough_json(vec![plain, shifted]);
        json.legend = legend(&[
            HighlightColor::Blue,
            HighlightColor::Red,
            HighlightColor::Green,
        ]);
        let walkthrough = load(json, &project, cx)
            .await
            .expect("walkthrough should be valid");

        assert_eq!(
            walkthrough
                .legend_label(HighlightColor::Red)
                .map(|label| label.as_ref()),
            Some("Meaning of red")
        );
        assert_eq!(walkthrough.legend_label(HighlightColor::Purple), None);
        assert_eq!(
            walkthrough.steps[0].colors(),
            [
                HighlightColor::Blue,
                HighlightColor::Red,
                HighlightColor::Green
            ]
        );
        assert_eq!(walkthrough.steps[1].colors(), [HighlightColor::Red]);

        cx.update(|cx| {
            let highlights = |step: &WalkthroughStep| {
                let snapshot = step.buffer.read(cx);
                step.highlights
                    .iter()
                    .map(|highlight| {
                        (
                            highlight.range.start.to_point(snapshot)
                                ..highlight.range.end.to_point(snapshot),
                            highlight.color,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                highlights(&walkthrough.steps[0]),
                [
                    (Point::new(3, 0)..Point::new(3, 11), HighlightColor::Red),
                    (Point::new(3, 4)..Point::new(3, 10), HighlightColor::Green),
                ]
            );
            assert_eq!(
                highlights(&walkthrough.steps[1]),
                [(Point::new(37, 5)..Point::new(37, 7), HighlightColor::Red)]
            );
        });
    }

    #[gpui::test]
    async fn test_color_validation_errors(cx: &mut TestAppContext) {
        let project = test_project(cx).await;

        let mut colored = step("colored", "root/src/one.rs", 10, 12);
        colored.color = Some(HighlightColor::Purple);
        colored.highlights = vec![
            highlight(9, 10, None, HighlightColor::Blue),
            highlight(11, 13, None, HighlightColor::Blue),
            highlight(12, 11, None, HighlightColor::Blue),
            highlight(10, 11, Some("line 12"), HighlightColor::Orange),
        ];
        let mut crowded = step("crowded", "root/src/one.rs", 20, 22);
        crowded.highlights = (0..9)
            .map(|_| highlight(20, 20, None, HighlightColor::Blue))
            .collect();

        let mut json = walkthrough_json(vec![colored, crowded]);
        json.legend = legend(&[HighlightColor::Blue, HighlightColor::Blue]);
        let errors = load(json, &project, cx)
            .await
            .err()
            .expect("walkthrough should be invalid");
        let rendered = errors
            .iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            [
                "the legend lists `blue` more than once",
                "the color `purple` is used but has no entry in `legend`",
                "the color `orange` is used but has no entry in `legend`",
                "Step `colored`: highlight lines 9-10 are not a valid range within the step's lines",
                "Step `colored`: highlight lines 11-13 are not a valid range within the step's lines",
                "Step `colored`: highlight lines 12-11 are not a valid range within the step's lines",
                "Step `colored`: highlight text `line 12` was not found in lines 10-11",
                "Step `crowded`: a step can have at most 8 highlights, but 9 were given",
            ]
        );
    }

    #[gpui::test]
    async fn test_step_data_changes_resolve(cx: &mut TestAppContext) {
        let project = test_project(cx).await;

        let mut first = step("first", "root/src/one.rs", 1, 2);
        first.data = vec![data("quantity", "2"), data("cart", "[]")];
        let mut second = step("second", "root/src/two.rs", 3, 5);
        second.data = vec![data("cart", "[{\"sku\": \"A-12\"}]"), data("quantity", "2")];
        let mut third = step("third", "root/src/one.rs", 10, 12);
        third.data = vec![data("cart", "[{\"sku\": \"A-12\"}]"), data("total", "24")];
        let plain = step("plain", "root/src/one.rs", 20, 21);

        let mut json = walkthrough_json(vec![first, second, plain, third]);
        json.example = Some("A user adds 2 units of A-12 to an empty cart".to_string());
        let walkthrough = load(json, &project, cx)
            .await
            .expect("walkthrough should be valid");

        assert_eq!(
            walkthrough.example.as_deref(),
            Some("A user adds 2 units of A-12 to an empty cart")
        );
        let summarize = |step: &WalkthroughStep| {
            step.data
                .iter()
                .map(|data| (data.name.to_string(), data.change.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            summarize(&walkthrough.steps[0]),
            [
                ("quantity".to_string(), DataChange::New),
                ("cart".to_string(), DataChange::New),
            ]
        );
        assert_eq!(
            summarize(&walkthrough.steps[1]),
            [
                (
                    "cart".to_string(),
                    DataChange::Changed {
                        previous: "[]".into()
                    }
                ),
                ("quantity".to_string(), DataChange::Unchanged),
            ]
        );
        assert!(walkthrough.steps[2].data.is_empty());
        assert_eq!(
            summarize(&walkthrough.steps[3]),
            [
                ("cart".to_string(), DataChange::Unchanged),
                ("total".to_string(), DataChange::New),
            ]
        );
    }

    #[gpui::test]
    async fn test_step_data_validation_errors(cx: &mut TestAppContext) {
        let project = test_project(cx).await;

        let mut crowded = step("crowded", "root/src/one.rs", 1, 2);
        crowded.data = (0..9)
            .map(|index| data(&format!("name{index}"), "1"))
            .collect();
        let mut malformed = step("malformed", "root/src/one.rs", 3, 4);
        malformed.data = vec![
            data("", "1"),
            data("  ", "1"),
            data("twice", "1"),
            data("twice", "2"),
            data("twice", "3"),
            data("long", &"x".repeat(MAX_DATA_VALUE_LENGTH + 1)),
            data("limit", &"é".repeat(MAX_DATA_VALUE_LENGTH)),
        ];

        let errors = load(walkthrough_json(vec![crowded, malformed]), &project, cx)
            .await
            .err()
            .expect("walkthrough should be invalid");
        assert_eq!(
            errors
                .iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>(),
            [
                "Step `crowded`: a step can have at most 8 data entries, but 9 were given",
                "Step `malformed`: a data entry has an empty `name`",
                "Step `malformed`: a data entry has an empty `name`",
                "Step `malformed`: the data name `twice` is used more than once in this step",
                "Step `malformed`: the value of data `long` is 401 characters, but at most 400 are allowed",
            ]
        );
    }

    #[gpui::test]
    async fn test_steps_outside_the_project(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/frontend"),
            json!({ "src": { "app.rs": numbered_lines(20) } }),
        )
        .await;
        fs.insert_tree(
            path!("/backend"),
            json!({ "src": { "server.rs": numbered_lines(30), "other.rs": numbered_lines(5) } }),
        )
        .await;
        let project = Project::test(fs, [path!("/frontend").as_ref()], cx).await;
        let visible_roots = |cx: &mut TestAppContext| {
            project.read_with(cx, |project, cx| {
                project
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                    .collect::<Vec<_>>()
            })
        };
        let roots_before = visible_roots(cx);

        let mut server = step("server", path!("/backend/src/server.rs"), 10, 12);
        server.anchor_text = Some("line 10".to_string());
        server.data = vec![data("status", "200")];
        let mut again = step("again", path!("/backend/src/server.rs"), 20, 21);
        again.highlights = vec![highlight(20, 20, None, HighlightColor::Blue)];
        let mut json = walkthrough_json(vec![
            step("app", "frontend/src/app.rs", 1, 2),
            server,
            again,
        ]);
        json.legend = legend(&[HighlightColor::Blue]);
        let walkthrough = load(json, &project, cx)
            .await
            .expect("steps outside the project should resolve");

        assert_eq!(visible_roots(cx), roots_before);
        assert_eq!(
            walkthrough
                .steps
                .iter()
                .map(|step| step.file_label.to_string())
                .collect::<Vec<_>>(),
            [
                "app.rs",
                path!("/backend/src/server.rs"),
                path!("/backend/src/server.rs")
            ]
        );
        cx.update(|cx| {
            let server = &walkthrough.steps[1];
            assert_eq!(server.point_range(cx).start.row, 9);
            assert_eq!(walkthrough.steps[2].highlights.len(), 1);
            let absolute_path = project.read(cx).absolute_path(&server.path, cx);
            assert_eq!(
                absolute_path.as_deref(),
                Some(Path::new(path!("/backend/src/server.rs")))
            );
        });
        assert_eq!(walkthrough.file_count(), 2);

        let errors = load(
            walkthrough_json(vec![
                step("missing", path!("/backend/src/missing.rs"), 1, 2),
                step("directory", path!("/backend/src"), 1, 2),
                step("nowhere", path!("/elsewhere/file.rs"), 1, 2),
                step("relative", "backend/src/other.rs", 1, 2),
            ]),
            &project,
            cx,
        )
        .await
        .err()
        .expect("only real files can be annotated");
        assert_eq!(
            error_kinds(errors),
            vec![
                (
                    Some("missing".to_string()),
                    StepErrorKind::PathNotFound {
                        path: path!("/backend/src/missing.rs").to_string()
                    }
                ),
                (
                    Some("directory".to_string()),
                    StepErrorKind::PathNotFound {
                        path: path!("/backend/src").to_string()
                    }
                ),
                (
                    Some("nowhere".to_string()),
                    StepErrorKind::PathNotFound {
                        path: path!("/elsewhere/file.rs").to_string()
                    }
                ),
                (
                    Some("relative".to_string()),
                    StepErrorKind::PathNotFound {
                        path: "backend/src/other.rs".to_string()
                    }
                ),
            ]
        );
        assert_eq!(visible_roots(cx), roots_before);

        let result = cx
            .update(|cx| {
                show_walkthrough(
                    walkthrough_json(vec![step("server", path!("/backend/src/other.rs"), 1, 2)]),
                    &project,
                    cx,
                )
            })
            .await;
        assert_eq!(
            result.as_deref(),
            Ok("Walkthrough shown to the user: 1 step across 1 file.")
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
                "node": "parse",
                "color": "blue",
                "highlights": [
                    { "start_line": 44, "end_line": 44, "text": "token", "color": "red" },
                    { "start_line": 50, "end_line": 52, "color": "blue" }
                ]
            }, {
                "id": "minimal",
                "path": "src/auth/middleware.rs",
                "start_line": 1,
                "end_line": 2,
                "title": "Minimal",
                "comment": "No optional fields."
            }],
            "flowchart": "flowchart TD\n  parse[Parse token] --> verify{Valid?}",
            "legend": [
                { "color": "blue", "label": "Input" },
                { "color": "red", "label": "Rejected" }
            ]
        }))
        .expect("spec example should deserialize");
        assert_eq!(json.steps[0].node.as_deref(), Some("parse"));
        assert_eq!(json.steps[0].color, Some(HighlightColor::Blue));
        assert_eq!(json.steps[0].highlights.len(), 2);
        assert_eq!(json.steps[0].highlights[1].text, None);
        assert_eq!(json.steps[1].anchor_text, None);
        assert_eq!(json.steps[1].color, None);
        assert!(json.steps[1].highlights.is_empty());
        assert_eq!(json.legend[1].color, HighlightColor::Red);
        assert_eq!(json.example, None);
        assert!(json.steps[0].data.is_empty());

        let with_data: WalkthroughJson = serde_json::from_value(json!({
            "title": "Cart",
            "summary": "Adding to a cart.",
            "example": "A user adds 2 units of A-12 to an empty cart",
            "steps": [{
                "id": "add",
                "path": "src/cart.rs",
                "start_line": 1,
                "end_line": 2,
                "title": "Add",
                "comment": "Adds.",
                "data": [{ "name": "quantity", "value": "2" }]
            }]
        }))
        .expect("example data should deserialize");
        assert_eq!(
            with_data.example.as_deref(),
            Some("A user adds 2 units of A-12 to an empty cart")
        );
        assert_eq!(with_data.steps[0].data, [data("quantity", "2")]);

        let fixture: WalkthroughJson =
            serde_json::from_str(include_str!("../test_fixtures/sample.json"))
                .expect("fixture should deserialize");
        assert!(fixture.flowchart.is_some());
    }

    #[gpui::test]
    async fn test_show_walkthrough_activates_valid_walkthrough(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));

        let invalid = walkthrough_json(vec![
            step("a", "root/src/missing.rs", 1, 2),
            step("b", "root/src/two.rs", 3, 5),
        ]);
        let result = cx
            .update(|cx| show_walkthrough(invalid, &project, cx))
            .await;
        assert_eq!(
            result.as_ref().map_err(String::as_str),
            Err(indoc! {"
                The walkthrough was not shown. Fix this problem and call the tool again:
                - Step `a`: `root/src/missing.rs` is not a file in the project"}
            .trim_end())
        );
        assert!(store.read_with(cx, |store, _| store.active().is_none()));

        let valid = walkthrough_json(vec![
            step("a", "root/src/one.rs", 1, 2),
            step("b", "root/src/two.rs", 3, 5),
        ]);
        let result = cx.update(|cx| show_walkthrough(valid, &project, cx)).await;
        assert_eq!(
            result.as_deref(),
            Ok("Walkthrough shown to the user: 2 steps across 2 files.")
        );
        assert!(store.read_with(cx, |store, _| store.active().is_some()));
    }

    fn titled(title: &str) -> WalkthroughJson {
        let mut json = walkthrough_json(vec![step("a", "root/src/one.rs", 1, 2)]);
        json.title = title.to_string();
        json
    }

    async fn show(
        json: &WalkthroughJson,
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let shown = cx
            .update(|cx| show_walkthrough(json.clone(), project, cx))
            .await;
        cx.run_until_parked();
        shown
    }

    fn history_titles(project: &Entity<Project>, cx: &mut TestAppContext) -> Vec<String> {
        let store = cx.update(|cx| WalkthroughStore::for_project(project, cx));
        store.read_with(cx, |store, _| {
            store
                .history()
                .iter()
                .map(|entry| entry.walkthrough.title.clone())
                .collect()
        })
    }

    #[gpui::test]
    async fn test_history_is_newest_first_without_duplicates(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let first = titled("First");
        let second = titled("Second");
        show(&first, &project, cx).await.expect("first is valid");
        show(&second, &project, cx).await.expect("second is valid");
        assert_eq!(history_titles(&project, cx), ["Second", "First"]);

        show(&first, &project, cx).await.expect("first is valid");
        assert_eq!(history_titles(&project, cx), ["First", "Second"]);
    }

    #[gpui::test]
    async fn test_history_is_capped(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        for number in 0..MAX_WALKTHROUGH_HISTORY + 2 {
            show(&titled(&format!("Walkthrough {number}")), &project, cx)
                .await
                .expect("walkthrough is valid");
        }
        let titles = history_titles(&project, cx);
        assert_eq!(titles.len(), MAX_WALKTHROUGH_HISTORY);
        assert_eq!(titles.first().map(String::as_str), Some("Walkthrough 11"));
        assert_eq!(titles.last().map(String::as_str), Some("Walkthrough 2"));
    }

    #[gpui::test]
    async fn test_invalid_walkthrough_is_not_recorded(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let mut invalid = titled("Invalid");
        invalid.steps[0].path = "root/src/missing.rs".to_string();
        show(&invalid, &project, cx)
            .await
            .expect_err("a missing file is invalid");
        assert!(history_titles(&project, cx).is_empty());
    }

    #[gpui::test]
    async fn test_history_is_read_back_by_a_fresh_project(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "src": { "one.rs": numbered_lines(10) } }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        show(&titled("Older"), &project, cx).await.expect("valid");
        show(&titled("Newer"), &project, cx).await.expect("valid");

        let other_roots = Project::test(fs.clone(), [path!("/root/src").as_ref()], cx).await;
        let other_store = cx.update(|cx| WalkthroughStore::for_project(&other_roots, cx));
        cx.update(|cx| other_store.update(cx, |store, cx| store.load_history(cx)))
            .await;
        assert!(history_titles(&other_roots, cx).is_empty());

        let fresh = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let fresh_store = cx.update(|cx| WalkthroughStore::for_project(&fresh, cx));
        assert!(fresh_store.read_with(cx, |store, _| store.history().is_empty()));
        cx.update(|cx| fresh_store.update(cx, |store, cx| store.load_history(cx)))
            .await;
        assert_eq!(history_titles(&fresh, cx), ["Newer", "Older"]);
    }

    fn three_step_json() -> WalkthroughJson {
        walkthrough_json(vec![
            step("a", "root/src/one.rs", 1, 2),
            step("b", "root/src/two.rs", 3, 5),
            step("c", "root/src/one.rs", 10, 12),
        ])
    }

    fn edit_step(
        id: &str,
        start_line: u32,
        end_line: u32,
        after: Option<&str>,
    ) -> WalkthroughStepEditJson {
        WalkthroughStepEditJson {
            step: step(id, "root/src/one.rs", start_line, end_line),
            after: after.map(str::to_string),
        }
    }

    async fn edit(
        edit: WalkthroughEditJson,
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let result = cx.update(|cx| edit_walkthrough(edit, project, cx)).await;
        cx.run_until_parked();
        result
    }

    fn active_ids(store: &Entity<WalkthroughStore>, cx: &mut TestAppContext) -> Vec<String> {
        store.read_with(cx, |store, _| {
            store
                .active()
                .map(|walkthrough| {
                    walkthrough
                        .steps
                        .iter()
                        .map(|step| step.id.to_string())
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    fn history_jsons(
        store: &Entity<WalkthroughStore>,
        cx: &mut TestAppContext,
    ) -> Vec<HistoryEntry> {
        store.read_with(cx, |store, _| store.history().to_vec())
    }

    #[gpui::test]
    async fn test_edit_replaces_a_step_in_place_and_keeps_the_current_step(
        cx: &mut TestAppContext,
    ) {
        let project = test_project(cx).await;
        show(&three_step_json(), &project, cx).await.expect("valid");
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        store.update(cx, |store, cx| store.set_active_step(&StepId::new("b"), cx));
        let events = record_events(&store, cx);

        let mut replacement = edit_step("a", 1, 2, None);
        replacement.step.comment = "A better comment".to_string();
        let message = edit(
            WalkthroughEditJson {
                steps: vec![replacement],
                ..Default::default()
            },
            &project,
            cx,
        )
        .await;
        assert_eq!(
            message.as_deref(),
            Ok("Walkthrough updated: 3 steps across 2 files. Step order: a, b, c")
        );

        assert_eq!(active_ids(&store, cx), ["a", "b", "c"]);
        store.read_with(cx, |store, _| {
            let walkthrough = store.active().expect("active");
            assert_eq!(walkthrough.steps[0].comment.as_ref(), "A better comment");
            assert_eq!(store.active_step().map(|step| step.id.as_str()), Some("b"));
            let json = store.active_json().expect("json");
            assert_eq!(json.steps[0].comment, "A better comment");
        });
        let events = events.borrow();
        assert!(matches!(
            events.as_slice(),
            [WalkthroughEvent::Updated {
                active_step_moved: false,
                flowchart_added: false,
                ..
            }]
        ));
    }

    #[gpui::test]
    async fn test_edit_inserts_appends_and_removes_steps(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        show(&three_step_json(), &project, cx).await.expect("valid");
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));

        let message = edit(
            WalkthroughEditJson {
                steps: vec![
                    edit_step("after-a", 20, 21, Some("a")),
                    edit_step("after-after-a", 30, 31, Some("after-a")),
                    edit_step("last", 40, 41, None),
                ],
                ..Default::default()
            },
            &project,
            cx,
        )
        .await;
        assert_eq!(
            message.as_deref(),
            Ok(
                "Walkthrough updated: 6 steps across 2 files. Step order: a, after-a, after-after-a, b, c, last"
            )
        );
        assert_eq!(
            active_ids(&store, cx),
            ["a", "after-a", "after-after-a", "b", "c", "last"]
        );

        let message = edit(
            WalkthroughEditJson {
                remove_steps: vec!["b".to_string(), "last".to_string()],
                ..Default::default()
            },
            &project,
            cx,
        )
        .await;
        assert_eq!(
            message.as_deref(),
            Ok(
                "Walkthrough updated: 4 steps across 1 file. Step order: a, after-a, after-after-a, c"
            )
        );
    }

    #[gpui::test]
    async fn test_edit_replaces_title_legend_and_other_fields(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let mut json = three_step_json();
        json.steps[0].color = Some(HighlightColor::Blue);
        json.legend = legend(&[HighlightColor::Blue]);
        show(&json, &project, cx).await.expect("valid");
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));

        edit(
            WalkthroughEditJson {
                title: Some("New title".to_string()),
                summary: Some("New summary".to_string()),
                example: Some("An example".to_string()),
                flowchart: Some("flowchart TD\n  a --> b".to_string()),
                legend: Some(vec![
                    LegendEntryJson {
                        color: HighlightColor::Blue,
                        label: "Entry".to_string(),
                    },
                    LegendEntryJson {
                        color: HighlightColor::Red,
                        label: "Error".to_string(),
                    },
                ]),
                ..Default::default()
            },
            &project,
            cx,
        )
        .await
        .expect("edit is valid");

        store.read_with(cx, |store, _| {
            let walkthrough = store.active().expect("active");
            assert_eq!(walkthrough.title.as_ref(), "New title");
            assert_eq!(walkthrough.summary.as_ref(), "New summary");
            assert_eq!(walkthrough.example.as_deref(), Some("An example"));
            assert_eq!(walkthrough.legend.len(), 2);
            assert_eq!(
                walkthrough
                    .flowchart
                    .as_ref()
                    .map(|flowchart| flowchart.node_ids.len()),
                Some(2)
            );
        });
    }

    #[gpui::test]
    async fn test_edit_removing_the_current_step_moves_to_the_clamped_index(
        cx: &mut TestAppContext,
    ) {
        let project = test_project(cx).await;
        show(&three_step_json(), &project, cx).await.expect("valid");
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        store.update(cx, |store, cx| store.set_active_step(&StepId::new("c"), cx));
        let events = record_events(&store, cx);

        edit(
            WalkthroughEditJson {
                remove_steps: vec!["c".to_string()],
                ..Default::default()
            },
            &project,
            cx,
        )
        .await
        .expect("edit is valid");
        store.read_with(cx, |store, _| {
            assert_eq!(store.active_step().map(|step| step.id.as_str()), Some("b"));
            assert_eq!(store.active_step_index(), Some(1));
        });
        assert!(matches!(
            events.borrow().as_slice(),
            [WalkthroughEvent::Updated {
                active_step_moved: true,
                ..
            }]
        ));

        store.update(cx, |store, cx| store.set_active_step(&StepId::new("a"), cx));
        edit(
            WalkthroughEditJson {
                remove_steps: vec!["a".to_string()],
                ..Default::default()
            },
            &project,
            cx,
        )
        .await
        .expect("edit is valid");
        store.read_with(cx, |store, _| {
            assert_eq!(store.active_step().map(|step| step.id.as_str()), Some("b"));
            assert_eq!(store.active_step_index(), Some(0));
        });
    }

    #[gpui::test]
    async fn test_failed_edit_changes_nothing(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        show(&three_step_json(), &project, cx).await.expect("valid");
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        store.update(cx, |store, cx| store.set_active_step(&StepId::new("b"), cx));
        let json_before = store.read_with(cx, |store, _| store.active_json().cloned());
        let history_before = history_jsons(&store, cx);
        let events = record_events(&store, cx);

        let mut broken = edit_step("broken", 1, 200, None);
        broken.step.node = Some("nowhere".to_string());
        let error = edit(
            WalkthroughEditJson {
                steps: vec![
                    edit_step("new", 5, 6, Some("missing")),
                    edit_step("a", 1, 2, Some("b")),
                    broken,
                ],
                remove_steps: vec!["gone".to_string()],
                ..Default::default()
            },
            &project,
            cx,
        )
        .await;
        assert_eq!(
            error.as_ref().map_err(String::as_str),
            Err(indoc! {"
                The walkthrough was not changed. Fix these 5 problems and call the tool again:
                - `remove_steps` lists `gone`, which is not a step id in the walkthrough
                - Step `new`: `after` is `missing`, which is not a step id in the walkthrough
                - Step `a`: this id already exists, so the step is replaced in place; leave out `after`
                - Step `broken`: lines 1-200 are not a valid range in a file with 101 lines (lines are 1-based and inclusive)
                - Step `broken`: `nowhere` is not a node id in the flowchart"}
            .trim_end())
        );

        assert_eq!(
            store.read_with(cx, |store, _| store.active_json().cloned()),
            json_before
        );
        assert_eq!(active_ids(&store, cx), ["a", "b", "c"]);
        store.read_with(cx, |store, _| {
            assert_eq!(store.active_step().map(|step| step.id.as_str()), Some("b"));
        });
        assert_eq!(history_jsons(&store, cx), history_before);
        assert!(events.borrow().is_empty());
    }

    #[gpui::test]
    async fn test_edit_removing_every_step_is_rejected(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        show(&titled("Only"), &project, cx).await.expect("valid");
        let error = edit(
            WalkthroughEditJson {
                remove_steps: vec!["a".to_string()],
                ..Default::default()
            },
            &project,
            cx,
        )
        .await;
        assert_eq!(
            error.as_ref().map_err(String::as_str),
            Err(indoc! {"
                The walkthrough was not changed. Fix this problem and call the tool again:
                - a walkthrough needs between 1 and 30 steps, but 0 were given"}
            .trim_end())
        );
    }

    #[gpui::test]
    async fn test_edit_without_changes_or_walkthrough_is_an_error(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let error = edit(
            WalkthroughEditJson {
                title: Some("Title".to_string()),
                ..Default::default()
            },
            &project,
            cx,
        )
        .await;
        assert_eq!(
            error.as_ref().map_err(String::as_str),
            Err("No walkthrough is active. Use create_walkthrough first.")
        );

        show(&three_step_json(), &project, cx).await.expect("valid");
        let error = edit(WalkthroughEditJson::default(), &project, cx).await;
        assert!(
            error.as_ref().is_err_and(|error| error
                .starts_with("The walkthrough was not changed: the edit sets no field")),
            "{error:?}"
        );
    }

    #[gpui::test]
    async fn test_edit_replaces_the_history_entry_it_was_made_from(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        show(&titled("Other"), &project, cx).await.expect("valid");
        show(&three_step_json(), &project, cx).await.expect("valid");
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        assert_eq!(history_titles(&project, cx), ["Title", "Other"]);

        edit(
            WalkthroughEditJson {
                title: Some("Edited".to_string()),
                ..Default::default()
            },
            &project,
            cx,
        )
        .await
        .expect("edit is valid");
        assert_eq!(history_titles(&project, cx), ["Edited", "Other"]);

        edit(
            WalkthroughEditJson {
                title: Some("Edited twice".to_string()),
                ..Default::default()
            },
            &project,
            cx,
        )
        .await
        .expect("edit is valid");
        let entries = history_jsons(&store, cx);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.walkthrough.title.as_str())
                .collect::<Vec<_>>(),
            ["Edited twice", "Other"]
        );
    }

    #[gpui::test]
    async fn test_describe_walkthrough_round_trips(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        assert_eq!(
            cx.update(|cx| describe_walkthrough(&project, cx)),
            "No walkthrough is active in this project."
        );

        let mut json = three_step_json();
        json.steps[1].highlights = vec![highlight(3, 3, Some("second"), HighlightColor::Red)];
        json.steps[1].color = Some(HighlightColor::Blue);
        json.legend = legend(&[HighlightColor::Blue, HighlightColor::Red]);
        json.example = Some("An example".to_string());
        json.steps[0].data = vec![data("count", "2")];
        show(&json, &project, cx).await.expect("valid");
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        store.update(cx, |store, cx| store.set_active_step(&StepId::new("c"), cx));

        let description = cx.update(|cx| describe_walkthrough(&project, cx));
        let (first_line, rest) = description.split_once('\n').expect("two parts");
        assert_eq!(first_line, "The user is on step \"c\" (3 of 3).");
        let parsed: WalkthroughJson = serde_json::from_str(rest).expect("JSON should parse");
        assert_eq!(parsed, json);

        store.update(cx, |store, cx| store.dismiss(cx));
        assert_eq!(
            cx.update(|cx| describe_walkthrough(&project, cx)),
            "No walkthrough is active in this project."
        );
    }
}
