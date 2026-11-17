use std::path::PathBuf;

#[cfg(not(unix))]
use gpui::{App, Entity};
#[cfg(not(unix))]
use project::Project;

pub(crate) const ZED_MCP_SERVER_NAME: &str = "zed";

/// The stdio command an external agent runs to reach Zed's in-process MCP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ZedMcpServerCommand {
    pub path: PathBuf,
    pub args: Vec<String>,
}

#[cfg(unix)]
pub(crate) use unix::zed_mcp_server_for_project;

/// Returns the stdio MCP server entry that lets an external agent reach
/// Zed's own tools for `project`, or `None` when it is unavailable.
#[cfg(not(unix))]
pub(crate) fn zed_mcp_server_for_project(
    _project: &Entity<Project>,
    _cx: &mut App,
) -> Option<ZedMcpServerCommand> {
    None
}

#[cfg(unix)]
mod unix {
    use super::{ZED_MCP_SERVER_NAME, ZedMcpServerCommand};
    use anyhow::{Context as _, Result, anyhow};
    use collections::HashMap;
    use context_server::listener::{McpServer, McpServerTool, ToolResponse};
    use context_server::types::{
        Implementation, InitializeParams, InitializeResponse, ProtocolVersion, ServerCapabilities,
        ToolAnnotations, ToolResponseContent, ToolsCapabilities, requests,
    };
    use feature_flags::{FeatureFlagAppExt as _, WalkthroughFeatureFlag};
    use gpui::{App, AsyncApp, Entity, Global, Task, WeakEntity};
    use project::Project;
    use util::ResultExt as _;

    #[derive(Clone)]
    struct CreateWalkthroughMcpTool {
        project: WeakEntity<Project>,
    }

    impl McpServerTool for CreateWalkthroughMcpTool {
        type Input = walkthrough::WalkthroughJson;
        type Output = ();

        const NAME: &'static str = "create_walkthrough";

        fn annotations(&self) -> ToolAnnotations {
            ToolAnnotations {
                title: Some("Create walkthrough".into()),
                read_only_hint: None,
                destructive_hint: Some(false),
                idempotent_hint: None,
                open_world_hint: Some(false),
            }
        }

        async fn run(
            &self,
            input: Self::Input,
            cx: &mut AsyncApp,
        ) -> Result<ToolResponse<Self::Output>> {
            let project = self
                .project
                .upgrade()
                .context("the project is no longer open")?;
            let shown = cx.update(|cx| walkthrough::show_walkthrough(input, &project, cx));
            match shown.await {
                Ok(message) => Ok(ToolResponse {
                    content: vec![ToolResponseContent::Text { text: message }],
                    structured_content: (),
                }),
                Err(error) => Err(anyhow!(error)),
            }
        }
    }

    struct ProjectServer {
        command: ZedMcpServerCommand,
        // Dropping the server stops its socket listener, so it is kept for as
        // long as the project is open.
        _server: McpServer,
    }

    #[derive(Default)]
    struct GlobalZedMcpServers(HashMap<WeakEntity<Project>, ProjectServer>);

    impl Global for GlobalZedMcpServers {}

    fn is_available(project: &Entity<Project>, cx: &App) -> bool {
        let project = project.read(cx);
        project.is_local()
            && cx.has_flag::<WalkthroughFeatureFlag>()
            && !project
                .context_server_store()
                .read(cx)
                .configured_server_ids()
                .iter()
                .any(|id| id.0.as_ref() == ZED_MCP_SERVER_NAME)
    }

    /// Returns the stdio MCP server entry that lets an external agent reach
    /// Zed's own tools for `project`, or `None` when it is unavailable.
    ///
    /// This is synchronous because session requests are built synchronously,
    /// and callers rely on a session being registered before they yield.
    pub(crate) fn zed_mcp_server_for_project(
        project: &Entity<Project>,
        cx: &mut App,
    ) -> Option<ZedMcpServerCommand> {
        if !is_available(project, cx) {
            return None;
        }

        let weak_project = project.downgrade();
        if let Some(existing) = cx
            .default_global::<GlobalZedMcpServers>()
            .0
            .get(&weak_project)
        {
            return Some(existing.command.clone());
        }

        let (server, command) = start_server(weak_project.clone(), cx).log_err()?;
        cx.default_global::<GlobalZedMcpServers>().0.insert(
            weak_project.clone(),
            ProjectServer {
                command: command.clone(),
                _server: server,
            },
        );
        cx.observe_release(project, move |_, cx| {
            cx.default_global::<GlobalZedMcpServers>()
                .0
                .remove(&weak_project);
        })
        .detach();
        Some(command)
    }

    fn start_server(
        project: WeakEntity<Project>,
        cx: &App,
    ) -> Result<(McpServer, ZedMcpServerCommand)> {
        let path = std::env::current_exe().context("finding the Zed executable path")?;
        let mut server = McpServer::new(cx)?;
        server.handle_request::<requests::Initialize>(handle_initialize);
        server.add_tool(CreateWalkthroughMcpTool { project });
        let args = vec![
            "--nc".to_string(),
            server.socket_path().display().to_string(),
        ];
        Ok((server, ZedMcpServerCommand { path, args }))
    }

    fn handle_initialize(params: InitializeParams, cx: &App) -> Task<Result<InitializeResponse>> {
        use context_server::types::{
            LATEST_PROTOCOL_VERSION, VERSION_2024_11_05, VERSION_2025_03_26, VERSION_2025_06_18,
        };

        let requested = params.protocol_version.0;
        let protocol_version = if matches!(
            requested.as_str(),
            VERSION_2024_11_05 | VERSION_2025_03_26 | VERSION_2025_06_18 | LATEST_PROTOCOL_VERSION
        ) {
            requested
        } else {
            LATEST_PROTOCOL_VERSION.to_string()
        };
        cx.foreground_executor().spawn(async move {
            Ok(InitializeResponse {
                protocol_version: ProtocolVersion(protocol_version),
                capabilities: ServerCapabilities {
                    experimental: None,
                    logging: None,
                    completions: None,
                    prompts: None,
                    resources: None,
                    tools: Some(ToolsCapabilities {
                        list_changed: Some(false),
                    }),
                },
                server_info: Implementation {
                    name: ZED_MCP_SERVER_NAME.into(),
                    title: None,
                    version: "0.1.0".into(),
                    description: None,
                },
                meta: None,
            })
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use fs::FakeFs;
        use gpui::TestAppContext;
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
                    "src": { "one.rs": "line 1\nline 2\nline 3\nline 4\n" }
                }),
            )
            .await;
            Project::test(fs, [path!("/root").as_ref()], cx).await
        }

        fn tool_input(path: &str) -> walkthrough::WalkthroughJson {
            serde_json::from_value(json!({
                "title": "T",
                "summary": "S",
                "steps": [{
                    "id": "a",
                    "path": path,
                    "start_line": 1,
                    "end_line": 2,
                    "title": "A",
                    "comment": "C"
                }]
            }))
            .expect("valid walkthrough input")
        }

        #[gpui::test]
        async fn test_create_walkthrough_tool_shows_walkthrough(cx: &mut TestAppContext) {
            let project = test_project(cx).await;
            let tool = CreateWalkthroughMcpTool {
                project: project.downgrade(),
            };

            let response = tool
                .run(tool_input("root/src/one.rs"), &mut cx.to_async())
                .await
                .expect("tool call succeeds");

            let [ToolResponseContent::Text { text }] = response.content.as_slice() else {
                panic!("expected a single text item, got {:?}", response.content);
            };
            assert!(
                text.starts_with("Walkthrough shown to the user"),
                "unexpected message: {text}"
            );
            let store = cx.update(|cx| walkthrough::WalkthroughStore::for_project(&project, cx));
            store.read_with(cx, |store, _| {
                assert!(store.active().is_some());
            });
        }

        #[gpui::test]
        async fn test_create_walkthrough_tool_reports_invalid_path(cx: &mut TestAppContext) {
            let project = test_project(cx).await;
            let tool = CreateWalkthroughMcpTool {
                project: project.downgrade(),
            };

            let error = tool
                .run(tool_input("root/src/missing.rs"), &mut cx.to_async())
                .await
                .expect_err("tool call fails");

            assert!(
                error
                    .to_string()
                    .starts_with("The walkthrough was not shown."),
                "unexpected error: {error}"
            );
            let store = cx.update(|cx| walkthrough::WalkthroughStore::for_project(&project, cx));
            store.read_with(cx, |store, _| {
                assert!(store.active().is_none());
            });
        }

        #[gpui::test]
        async fn test_server_for_project_is_cached_and_listens(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let project = test_project(cx).await;

            let command = cx
                .update(|cx| zed_mcp_server_for_project(&project, cx))
                .expect("a server command");
            assert!(command.path.is_absolute());
            let [flag, socket_path] = command.args.as_slice() else {
                panic!("unexpected args: {:?}", command.args);
            };
            assert_eq!(flag, "--nc");
            assert!(std::path::Path::new(socket_path).exists());

            let second = cx
                .update(|cx| zed_mcp_server_for_project(&project, cx))
                .expect("a cached server command");
            assert_eq!(second, command);

            drop(project);
            cx.run_until_parked();
        }
    }
}
