use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::v2::schema as acp;
use gpui::{App, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use walkthrough::{GlobalFlowchartCheck, Walkthrough, WalkthroughJson, WalkthroughStore};

/// The tool's description is the doc comment on [`WalkthroughJson`], which is
/// shared with the walkthrough fixture loader.
pub type CreateWalkthroughToolInput = WalkthroughJson;

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CreateWalkthroughToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<CreateWalkthroughToolOutput> for LanguageModelToolResultContent {
    fn from(output: CreateWalkthroughToolOutput) -> Self {
        match output {
            CreateWalkthroughToolOutput::Success { message } => message.into(),
            CreateWalkthroughToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct CreateWalkthroughTool {
    project: Entity<Project>,
}

impl CreateWalkthroughTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for CreateWalkthroughTool {
    type Input = CreateWalkthroughToolInput;
    type Output = CreateWalkthroughToolOutput;

    const NAME: &'static str = "create_walkthrough";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => format!("Create walkthrough “{}”", input.title).into(),
            Err(_) => "Create walkthrough".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|error| CreateWalkthroughToolOutput::Error {
                    error: error.to_string(),
                })?;

            let load = cx.update(|cx| {
                // Without a UI layer there is no renderer to validate
                // against, and nothing that would later fail to draw it.
                let flowchart_check = cx
                    .try_global::<GlobalFlowchartCheck>()
                    .map(|check| check.0.clone());
                Walkthrough::from_json(
                    input,
                    &project,
                    move |source| match flowchart_check {
                        Some(check) => check(source),
                        None => Ok(()),
                    },
                    cx,
                )
            });

            let walkthrough = load.await.map_err(|errors| {
                let mut error = format!(
                    "The walkthrough was not shown. Fix {} and call the tool again:",
                    if errors.len() == 1 {
                        "this problem".to_string()
                    } else {
                        format!("these {} problems", errors.len())
                    }
                );
                for step_error in errors {
                    error.push_str(&format!("\n- {step_error}"));
                }
                CreateWalkthroughToolOutput::Error { error }
            })?;

            let step_count = walkthrough.steps.len();
            let file_count = walkthrough.file_count();
            cx.update(|cx| {
                WalkthroughStore::for_project(&project, cx)
                    .update(cx, |store, cx| store.set_walkthrough(walkthrough, cx));
            });

            Ok(CreateWalkthroughToolOutput::Success {
                message: format!(
                    "Walkthrough shown to the user: {step_count} {} across {file_count} {}.",
                    if step_count == 1 { "step" } else { "steps" },
                    if file_count == 1 { "file" } else { "files" },
                ),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "src": {
                    "auth.rs": "fn authenticate() {\n    parse();\n    verify();\n}\n",
                    "handler.rs": "fn handle() {\n    respond();\n}\n",
                }
            }),
        )
        .await;
        Project::test(fs, [path!("/root").as_ref()], cx).await
    }

    fn valid_input() -> serde_json::Value {
        json!({
            "title": "How request auth works",
            "summary": "A request passes through authentication before the handler runs.",
            "steps": [
                {
                    "id": "authenticate",
                    "path": "root/src/auth.rs",
                    "start_line": 1,
                    "end_line": 4,
                    "anchor_text": "fn authenticate(",
                    "title": "Authenticate the request",
                    "comment": "Parses and verifies the token.",
                    "node": "auth"
                },
                {
                    "id": "verify",
                    "path": "root/src/auth.rs",
                    "start_line": 3,
                    "end_line": 3,
                    "title": "Verify",
                    "comment": "Rejects invalid tokens."
                },
                {
                    "id": "handle",
                    "path": "root/src/handler.rs",
                    "start_line": 1,
                    "end_line": 3,
                    "title": "Run the handler",
                    "comment": "Only reached for valid tokens.",
                    "node": "handler"
                }
            ],
            "flowchart": "flowchart TD\n  auth[Authenticate] --> handler[Run handler]"
        })
    }

    async fn run_tool(
        input: serde_json::Value,
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let input: CreateWalkthroughToolInput =
            serde_json::from_value(input).expect("input should deserialize");
        let tool = Arc::new(CreateWalkthroughTool::new(project.clone()));
        let (event_stream, _receiver) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await;
        match result {
            Ok(CreateWalkthroughToolOutput::Success { message }) => Ok(message),
            Err(CreateWalkthroughToolOutput::Error { error }) => Err(error),
            Ok(CreateWalkthroughToolOutput::Error { error }) => {
                panic!("error returned as success: {error}")
            }
            Err(CreateWalkthroughToolOutput::Success { message }) => {
                panic!("success returned as error: {message}")
            }
        }
    }

    #[gpui::test]
    async fn test_valid_input_creates_walkthrough(cx: &mut TestAppContext) {
        let project = test_project(cx).await;

        let message = run_tool(valid_input(), &project, cx).await;
        assert_eq!(
            message.as_deref(),
            Ok("Walkthrough shown to the user: 3 steps across 2 files.")
        );

        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        store.read_with(cx, |store, _| {
            let walkthrough = store.active().expect("walkthrough should be active");
            assert_eq!(walkthrough.title.as_ref(), "How request auth works");
            assert_eq!(walkthrough.steps.len(), 3);
            assert_eq!(
                walkthrough
                    .flowchart
                    .as_ref()
                    .map(|flowchart| flowchart.node_ids.len()),
                Some(2)
            );
            assert_eq!(
                store.active_step().map(|step| step.id.as_str()),
                Some("authenticate")
            );
        });
    }

    #[gpui::test]
    async fn test_validation_failures_are_returned_together(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        cx.update(|cx| {
            cx.set_global(GlobalFlowchartCheck(Arc::new(|source| {
                if source.contains("not mermaid") {
                    Err("unexpected token".to_string())
                } else {
                    Ok(())
                }
            })));
        });

        let mut input = valid_input();
        input["steps"][0]["path"] = json!("root/src/missing.rs");
        input["steps"][1]["end_line"] = json!(40);
        input["steps"][2]["node"] = json!("nowhere");
        input["flowchart"] = json!("not mermaid");

        let error = run_tool(input, &project, cx).await;
        assert_eq!(
            error.as_ref().map_err(String::as_str),
            Err(indoc::indoc! {"
                The walkthrough was not shown. Fix these 4 problems and call the tool again:
                - Step `authenticate`: `root/src/missing.rs` is not a file in the project
                - Step `verify`: lines 3-40 are not a valid range in a file with 5 lines (lines are 1-based and inclusive)
                - the flowchart could not be parsed: unexpected token
                - Step `handle`: `nowhere` is not a node id in the flowchart"}
            .trim_end())
        );

        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        assert!(store.read_with(cx, |store, _| store.active().is_none()));

        let mut input = valid_input();
        input["steps"][0]["anchor_text"] = json!("fn elsewhere(");
        let error = run_tool(input, &project, cx).await;
        assert_eq!(
            error.as_ref().map_err(String::as_str),
            Err(
                "The walkthrough was not shown. Fix this problem and call the tool again:\n- Step `authenticate`: anchor_text `fn elsewhere(` was not found within 20 lines of line 1; re-read the file and correct the line numbers"
            )
        );
    }
}
