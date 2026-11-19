use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::v2::schema as acp;
use gpui::{App, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::Project;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use walkthrough::GetWalkthroughJson;

/// The tool's description is the doc comment on [`GetWalkthroughJson`].
pub type GetWalkthroughToolInput = GetWalkthroughJson;

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GetWalkthroughToolOutput {
    Success { message: String },
    Error { error: String },
}

impl From<GetWalkthroughToolOutput> for LanguageModelToolResultContent {
    fn from(output: GetWalkthroughToolOutput) -> Self {
        match output {
            GetWalkthroughToolOutput::Success { message } => message.into(),
            GetWalkthroughToolOutput::Error { error } => error.into(),
        }
    }
}

pub struct GetWalkthroughTool {
    project: Entity<Project>,
}

impl GetWalkthroughTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for GetWalkthroughTool {
    type Input = GetWalkthroughToolInput;
    type Output = GetWalkthroughToolOutput;

    const NAME: &'static str = "get_walkthrough";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Read walkthrough".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            input
                .recv()
                .await
                .map_err(|error| GetWalkthroughToolOutput::Error {
                    error: error.to_string(),
                })?;

            let message = cx.update(|cx| walkthrough::describe_walkthrough(&project, cx));
            Ok(GetWalkthroughToolOutput::Success { message })
        })
    }
}

#[cfg(test)]
mod walkthrough_get_tests {
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

    async fn run_tool(project: &Entity<Project>, cx: &mut TestAppContext) -> String {
        let tool = Arc::new(GetWalkthroughTool::new(project.clone()));
        let (event_stream, _receiver) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(GetWalkthroughJson {}), event_stream, cx))
            .await;
        match result {
            Ok(GetWalkthroughToolOutput::Success { message }) => message,
            Ok(GetWalkthroughToolOutput::Error { error })
            | Err(GetWalkthroughToolOutput::Error { error }) => {
                panic!("unexpected error: {error}")
            }
            Err(GetWalkthroughToolOutput::Success { message }) => {
                panic!("success returned as error: {message}")
            }
        }
    }

    #[gpui::test]
    async fn test_reports_when_no_walkthrough_is_active(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        assert_eq!(
            run_tool(&project, cx).await,
            "No walkthrough is active in this project."
        );
    }

    #[gpui::test]
    async fn test_returns_the_active_walkthrough(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let input: WalkthroughJson = serde_json::from_value(json!({
            "title": "Auth",
            "summary": "How auth works.",
            "steps": [
                {
                    "id": "authenticate", "path": "root/src/auth.rs",
                    "start_line": 1, "end_line": 4,
                    "title": "Authenticate", "comment": "Parses and verifies."
                },
                {
                    "id": "verify", "path": "root/src/auth.rs",
                    "start_line": 3, "end_line": 3,
                    "title": "Verify", "comment": "Rejects invalid tokens."
                }
            ]
        }))
        .expect("input should deserialize");
        cx.update(|cx| walkthrough::show_walkthrough(input.clone(), &project, cx))
            .await
            .expect("walkthrough should be shown");
        let store = cx.update(|cx| WalkthroughStore::for_project(&project, cx));
        store.update(cx, |store, cx| {
            store.set_active_step(&walkthrough::StepId::new("verify"), cx)
        });

        let message = run_tool(&project, cx).await;
        let (first_line, json_text) = message.split_once('\n').expect("two parts");
        assert_eq!(first_line, "The user is on step \"verify\" (2 of 2).");
        let parsed: WalkthroughJson = serde_json::from_str(json_text).expect("JSON should parse");
        assert_eq!(parsed, input);
    }
}
