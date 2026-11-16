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
