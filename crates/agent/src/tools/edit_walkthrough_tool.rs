use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::v2::schema as acp;
use gpui::{App, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use walkthrough::WalkthroughEditJson;

/// The tool's description is the doc comment on [`WalkthroughEditJson`].
pub type EditWalkthroughToolInput = WalkthroughEditJson;

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EditWalkthroughToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<EditWalkthroughToolOutput> for LanguageModelToolResultContent {
    fn from(output: EditWalkthroughToolOutput) -> Self {
        match output {
            EditWalkthroughToolOutput::Success { message } => message.into(),
            EditWalkthroughToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct EditWalkthroughTool {
    project: Entity<Project>,
}

impl EditWalkthroughTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for EditWalkthroughTool {
    type Input = EditWalkthroughToolInput;
    type Output = EditWalkthroughToolOutput;

    const NAME: &'static str = "edit_walkthrough";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Edit walkthrough".into()
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
                .map_err(|error| EditWalkthroughToolOutput::Error {
                    error: error.to_string(),
                })?;

            cx.update(|cx| walkthrough::edit_walkthrough(input, &project, cx))
                .await
                .map(|message| EditWalkthroughToolOutput::Success { message })
                .map_err(|error| EditWalkthroughToolOutput::Error { error })
        })
    }
}

#[cfg(test)]
mod walkthrough_edit_tests {
    use super::*;
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use walkthrough::{WalkthroughJson, WalkthroughStore};

    async fn test_project(cx: &mut TestAppContext) -> Entity<Project> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "src": { "auth.rs": "fn authenticate() {\n    parse();\n    verify();\n}\n" } }),
        )
        .await;
        Project::test(fs, [path!("/root").as_ref()], cx).await
    }

    fn step(id: &str, start_line: u32, end_line: u32) -> serde_json::Value {
        json!({
            "id": id, "path": "root/src/auth.rs",
            "start_line": start_line, "end_line": end_line,
            "title": id, "comment": format!("About {id}.")
        })
    }

    async fn create(project: &Entity<Project>, cx: &mut TestAppContext) {
        let input: WalkthroughJson = serde_json::from_value(json!({
            "title": "Auth",
            "summary": "How auth works.",
            "steps": [step("authenticate", 1, 4), step("parse", 2, 2), step("verify", 3, 3)]
        }))
        .expect("input should deserialize");
        cx.update(|cx| walkthrough::show_walkthrough(input, project, cx))
            .await
            .expect("walkthrough should be shown");
    }

    async fn run_tool(
        input: serde_json::Value,
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> Result<String, String> {
        let input: EditWalkthroughToolInput =
            serde_json::from_value(input).expect("input should deserialize");
        let tool = Arc::new(EditWalkthroughTool::new(project.clone()));
        let (event_stream, _receiver) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await;
        match result {
            Ok(EditWalkthroughToolOutput::Success { message }) => Ok(message),
            Err(EditWalkthroughToolOutput::Error { error }) => Err(error),
            Ok(EditWalkthroughToolOutput::Error { error }) => {
                panic!("error returned as success: {error}")
            }
            Err(EditWalkthroughToolOutput::Success { message }) => {
                panic!("success returned as error: {message}")
            }
        }
    }

    #[gpui::test]
    async fn test_edit_without_a_walkthrough_is_an_error(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let error = run_tool(json!({ "title": "New" }), &project, cx).await;
        assert_eq!(
            error.as_ref().map_err(String::as_str),
            Err("No walkthrough is active. Use create_walkthrough first.")
        );
    }

    #[gpui::test]
    async fn test_edit_updates_the_walkthrough_in_place(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        create(&project, cx).await;
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        store.update(cx, |store, cx| {
            store.set_active_step(&walkthrough::StepId::new("parse"), cx)
        });

        let mut inserted = step("check", 3, 3);
        inserted["after"] = json!("parse");
        let message = run_tool(
            json!({ "title": "Renamed", "steps": [inserted], "remove_steps": ["verify"] }),
            &project,
            cx,
        )
        .await;
        assert_eq!(
            message.as_deref(),
            Ok(
                "Walkthrough updated: 3 steps across 1 file. Step order: authenticate, parse, check"
            )
        );

        store.read_with(cx, |store, _| {
            let walkthrough = store.active().expect("walkthrough should be active");
            assert_eq!(walkthrough.title.as_ref(), "Renamed");
            assert_eq!(
                store.active_step().map(|step| step.id.as_str()),
                Some("parse")
            );
        });
    }

    #[gpui::test]
    async fn test_edit_problems_are_returned_together(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        create(&project, cx).await;

        let mut inserted = step("check", 3, 3);
        inserted["after"] = json!("nowhere");
        let error = run_tool(
            json!({
                "steps": [inserted, step("parse", 2, 40)],
                "remove_steps": ["missing"]
            }),
            &project,
            cx,
        )
        .await;
        assert_eq!(
            error.as_ref().map_err(String::as_str),
            Err(indoc::indoc! {"
                The walkthrough was not changed. Fix these 3 problems and call the tool again:
                - `remove_steps` lists `missing`, which is not a step id in the walkthrough
                - Step `check`: `after` is `nowhere`, which is not a step id in the walkthrough
                - Step `parse`: lines 2-40 are not a valid range in a file with 5 lines (lines are 1-based and inclusive)"}
            .trim_end())
        );

        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        store.read_with(cx, |store, _| {
            let walkthrough = store.active().expect("walkthrough should be active");
            assert_eq!(walkthrough.steps.len(), 3);
            assert_eq!(walkthrough.title.as_ref(), "Auth");
        });
    }
}
