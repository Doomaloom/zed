mod editor_addon;
mod flowchart_view;
mod navigation;

use gpui::App;

pub use flowchart_view::{FlowchartView, parse_check};

pub fn init(cx: &mut App) {
    editor_addon::init(cx);
    navigation::init(cx);
    flowchart_view::init(cx);
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::ops::Range;

    use editor::Editor;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use language::Point;
    use project::{FakeFs, Project, ProjectPath};
    use serde_json::json;
    use util::{path, rel_path::rel_path};
    use walkthrough::{WalkthroughJson, WalkthroughStepJson, WalkthroughStore};
    use workspace::{AppState, MultiWorkspace, Workspace};

    pub struct TestWorkspace {
        pub project: Entity<Project>,
        pub workspace: Entity<Workspace>,
        pub store: Entity<WalkthroughStore>,
    }

    fn numbered_lines(count: usize) -> String {
        (1..=count)
            .map(|line| format!("line {line}\n"))
            .collect::<String>()
    }

    pub async fn init_test(cx: &mut TestAppContext) -> (TestWorkspace, &mut VisualTestContext) {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            command_palette_hooks::init(cx);
            editor::init(cx);
            crate::init(cx);
            app_state
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "src": {
                    "one.rs": numbered_lines(40),
                    "two.rs": numbered_lines(20),
                    "three.rs": numbered_lines(10),
                }
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = cx.read(|cx| multi_workspace.read(cx).workspace().clone());
        let store = cx.update(|_, cx| WalkthroughStore::for_project(&project, cx));
        (
            TestWorkspace {
                project,
                workspace,
                store,
            },
            cx,
        )
    }

    fn step(id: &str, path: &str, lines: Range<u32>, node: Option<&str>) -> WalkthroughStepJson {
        WalkthroughStepJson {
            id: id.to_string(),
            path: path.to_string(),
            start_line: lines.start,
            end_line: lines.end,
            anchor_text: None,
            title: format!("Step {id}"),
            comment: format!("Comment for **{id}**."),
            node: node.map(str::to_string),
        }
    }

    /// Steps `a` (one.rs, lines 2-4), `b` (two.rs, lines 5-6) and `c`
    /// (one.rs, lines 10-12). Only `a` and `b` are linked to flowchart nodes.
    pub fn sample_walkthrough(with_flowchart: bool) -> WalkthroughJson {
        let node = |node| with_flowchart.then_some(node);
        WalkthroughJson {
            title: "Sample".to_string(),
            summary: "A sample walkthrough.".to_string(),
            steps: vec![
                step("a", "root/src/one.rs", 2..4, node("parse")),
                step("b", "root/src/two.rs", 5..6, node("verify")),
                step("c", "root/src/one.rs", 10..12, None),
            ],
            flowchart: with_flowchart
                .then(|| "flowchart TD\n  parse[Parse token] --> verify{Valid?}".to_string()),
        }
    }

    pub async fn load_walkthrough(
        json: WalkthroughJson,
        test: &TestWorkspace,
        cx: &mut VisualTestContext,
    ) {
        let project = test.project.clone();
        let load = cx.update(|_, cx| {
            cx.spawn(async move |cx| {
                crate::navigation::load_walkthrough_json(json, &project, cx).await
            })
        });
        load.await.expect("walkthrough should load");
        cx.run_until_parked();
    }

    pub async fn open_editor(
        file_name: &str,
        test: &TestWorkspace,
        cx: &mut VisualTestContext,
    ) -> Entity<Editor> {
        let path = project_path(file_name, test, cx);
        let open = test.workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_path(path, None, true, window, cx)
        });
        let editor = open
            .await
            .expect("file should open")
            .downcast::<Editor>()
            .expect("item should be an editor");
        cx.run_until_parked();
        editor
    }

    pub fn project_path(
        file_name: &str,
        test: &TestWorkspace,
        cx: &mut VisualTestContext,
    ) -> ProjectPath {
        let worktree_id = test.project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("project should have a worktree")
                .read(cx)
                .id()
        });
        ProjectPath {
            worktree_id,
            path: rel_path(&format!("src/{file_name}")).into(),
        }
    }

    pub fn active_editor(test: &TestWorkspace, cx: &mut VisualTestContext) -> Entity<Editor> {
        test.workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("an editor should be active")
        })
    }

    pub fn active_step(test: &TestWorkspace, cx: &mut VisualTestContext) -> Option<String> {
        test.store.read_with(cx, |store, _| {
            store.active_step().map(|step| step.id.to_string())
        })
    }

    pub fn cursor(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor.selections.newest::<Point>(&snapshot).head()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use agent::{
        AgentTool as _, CreateWalkthroughTool, CreateWalkthroughToolOutput, ToolCallEventStream,
        ToolInput,
    };
    use editor::Editor;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use util::path;
    use walkthrough::{NextStep, WalkthroughJson, WalkthroughStore};
    use workspace::{AppState, MultiWorkspace, item::ItemHandle as _};

    use crate::FlowchartView;

    fn insert_file(tree: &mut serde_json::Value, path: &str, contents: String) {
        let mut node = tree;
        for component in path.split('/') {
            node = &mut node[component];
        }
        *node = serde_json::Value::String(contents);
    }

    /// Feeds the sample fixture, which annotates this feature's own source,
    /// through the agent tool and checks every UI piece reacts.
    #[gpui::test]
    async fn test_fixture_through_tool_end_to_end(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            command_palette_hooks::init(cx);
            editor::init(cx);
            crate::init(cx);
            app_state
        });

        let fixture: WalkthroughJson =
            serde_json::from_str(include_str!("../../walkthrough/test_fixtures/sample.json"))
                .expect("fixture should deserialize");
        let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut tree = serde_json::json!({});
        for step in &fixture.steps {
            let contents = std::fs::read_to_string(repository_root.join(&step.path))
                .unwrap_or_else(|error| panic!("reading {}: {error}", step.path));
            insert_file(&mut tree, &step.path, contents);
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/zed"), tree).await;
        let project = Project::test(fs, [path!("/zed").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = cx.read(|cx| multi_workspace.read(cx).workspace().clone());

        let tool = Arc::new(CreateWalkthroughTool::new(project.clone()));
        let (event_stream, _event_receiver) = ToolCallEventStream::test();
        let output = cx
            .update(|_, cx| tool.run(ToolInput::resolved(fixture.clone()), event_stream, cx))
            .await;
        let message = match output {
            Ok(CreateWalkthroughToolOutput::Success { message }) => message,
            Ok(CreateWalkthroughToolOutput::Error { error })
            | Err(CreateWalkthroughToolOutput::Error { error }) => {
                panic!("the fixture should be valid, regenerate it if the code moved:\n{error}")
            }
            Err(CreateWalkthroughToolOutput::Success { message }) => {
                panic!("success returned as an error: {message}")
            }
        };
        assert_eq!(
            message,
            "Walkthrough shown to the user: 9 steps across 5 files."
        );
        cx.run_until_parked();

        // Store state.
        let store = cx.update(|_, cx| WalkthroughStore::for_project(&project, cx));
        store.read_with(cx, |store, cx| {
            let walkthrough = store.active().expect("walkthrough should be active");
            assert_eq!(walkthrough.steps.len(), fixture.steps.len());
            assert_eq!(
                store.active_step().map(|step| step.id.as_str()),
                Some("tool-run")
            );
            for (step, step_json) in walkthrough.steps.iter().zip(&fixture.steps) {
                let buffer = step.buffer.read(cx);
                let range = step.point_range(cx);
                let first_line = buffer
                    .text_for_range(
                        language::Point::new(range.start.row, 0)
                            ..language::Point::new(range.start.row, buffer.line_len(range.start.row)),
                    )
                    .collect::<String>();
                let anchor_text = step_json.anchor_text.as_deref().expect("anchor text");
                assert!(
                    first_line.contains(anchor_text),
                    "step `{}` should start on the line containing `{anchor_text}`, found `{first_line}`",
                    step.id
                );
                assert!(!step.is_stale(cx));
            }
        });

        // Editor blocks: the first step's file is open, focused and annotated.
        let steps_in = |path: &str| {
            fixture
                .steps
                .iter()
                .filter(|step| step.path == path)
                .count()
        };
        let active_editor = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_item_as::<Editor>(cx)
                    .expect("an editor should be active")
            })
        };
        let editor_path = |editor: &gpui::Entity<Editor>, cx: &mut gpui::VisualTestContext| {
            editor
                .read_with(cx, |_, cx| editor.project_path(cx))
                .map(|path| path.path.as_unix_str().to_string())
        };
        let tool_editor = active_editor(cx);
        let tool_path = fixture.steps[0].path.as_str();
        assert_eq!(editor_path(&tool_editor, cx).as_deref(), Some(tool_path));
        assert_eq!(
            tool_editor.read_with(cx, |editor, _| crate::editor_addon::step_block_count(
                editor
            )),
            steps_in(tool_path)
        );

        // Flowchart view: open beside the code, with the first node styled.
        let flowchart = workspace
            .read_with(cx, |workspace, cx| {
                workspace.item_of_type::<FlowchartView>(cx)
            })
            .expect("flowchart should be open");
        flowchart.read_with(cx, |flowchart, _| {
            let source = flowchart.current_source().expect("source");
            assert!(source.contains("\n  style tool fill:#"), "{source}");
            assert_eq!(
                flowchart
                    .rendered_node_ids()
                    .expect("diagram should be rendered"),
                [
                    "tool",
                    "validate",
                    "errors",
                    "anchor",
                    "store",
                    "editor",
                    "reveal",
                    "diagram",
                    "nodeclick"
                ]
            );
        });

        // Stepping moves all three together.
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        let model_editor = active_editor(cx);
        let model_path = fixture.steps[1].path.as_str();
        assert_eq!(editor_path(&model_editor, cx).as_deref(), Some(model_path));
        assert_eq!(
            model_editor.read_with(cx, |editor, _| crate::editor_addon::step_block_count(
                editor
            )),
            steps_in(model_path)
        );
        flowchart.read_with(cx, |flowchart, _| {
            let source = flowchart.current_source().expect("source");
            assert!(source.contains("\n  style validate fill:#"), "{source}");
        });
    }
}
