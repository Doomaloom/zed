use std::{any::TypeId, path::Path};

use anyhow::{Context as _, Result, anyhow};
use command_palette_hooks::CommandPaletteFilter;
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use feature_flags::{FeatureFlagAppExt as _, WalkthroughFeatureFlag};
use gpui::{App, AsyncApp, Context, Entity, PathPromptOptions, Window, prelude::*};
use project::{Project, ProjectPath};
use walkthrough::{
    Dismiss, GoToStep, LoadFromJson, NextStep, PreviousStep, Reopen, StepId, ToggleFlowchart,
    WalkthroughEvent, WalkthroughJson, WalkthroughStore, activate_walkthrough,
};
use workspace::{
    Pane, SplitDirection, Toast, Workspace,
    notifications::{DetachAndPromptErr as _, NotificationId},
};

use crate::{FlowchartView, reopen_picker::ReopenPicker};

/// How many lines of context to keep above a step's range when scrolling to
/// it, so the range comes first and its comment block below stays in view.
const LINES_ABOVE_STEP: f64 = 3.;

pub fn init(cx: &mut App) {
    refresh_action_visibility(false, cx);
    cx.observe_flag::<WalkthroughFeatureFlag, _>(|_, cx| {
        let has_walkthrough = cx
            .try_global::<ActiveWalkthroughCount>()
            .is_some_and(|count| count.0 > 0);
        refresh_action_visibility(has_walkthrough, cx);
    })
    .detach();
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        register(workspace, window, cx);
    })
    .detach();
}

/// The command palette filter is global, so the walkthrough actions stay
/// listed while any workspace has a walkthrough.
#[derive(Default)]
struct ActiveWalkthroughCount(usize);

impl gpui::Global for ActiveWalkthroughCount {}

fn refresh_action_visibility(has_walkthrough: bool, cx: &mut App) {
    let walkthrough_actions = [
        TypeId::of::<NextStep>(),
        TypeId::of::<PreviousStep>(),
        TypeId::of::<GoToStep>(),
        TypeId::of::<ToggleFlowchart>(),
        TypeId::of::<Dismiss>(),
    ];
    let load_actions = [TypeId::of::<LoadFromJson>(), TypeId::of::<Reopen>()];
    let enabled = cx.has_flag::<WalkthroughFeatureFlag>();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if has_walkthrough {
            filter.show_action_types(&walkthrough_actions);
        } else {
            filter.hide_action_types(&walkthrough_actions);
        }
        if enabled {
            filter.show_action_types(&load_actions);
        } else {
            filter.hide_action_types(&load_actions);
        }
    });
}

fn register(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let store = WalkthroughStore::for_project(workspace.project(), cx);

    cx.subscribe_in(&store, window, {
        let mut had_walkthrough = false;
        move |workspace, store, event: &WalkthroughEvent, window, cx| {
            let has_walkthrough = store.read(cx).active().is_some();
            if has_walkthrough != had_walkthrough {
                had_walkthrough = has_walkthrough;
                let count = cx.default_global::<ActiveWalkthroughCount>();
                if has_walkthrough {
                    count.0 += 1;
                } else {
                    count.0 = count.0.saturating_sub(1);
                }
                let any_walkthrough = count.0 > 0;
                refresh_action_visibility(any_walkthrough, cx);
            }
            match event {
                WalkthroughEvent::Created(_) | WalkthroughEvent::ActiveStepChanged { .. } => {
                    reveal_active_step(workspace, store, window, cx);
                }
                WalkthroughEvent::Updated {
                    active_step_moved: true,
                    ..
                } => reveal_active_step(workspace, store, window, cx),
                WalkthroughEvent::Updated { .. } | WalkthroughEvent::Dismissed => {}
            }
        }
    })
    .detach();

    workspace.register_action_renderer({
        let store = store.clone();
        move |div, _, _, cx| {
            let div = if cx.has_flag::<WalkthroughFeatureFlag>() {
                div.on_action(cx.listener(|workspace, _: &LoadFromJson, window, cx| {
                    load_from_json(workspace, window, cx);
                }))
                .on_action(cx.listener(|workspace, _: &Reopen, window, cx| {
                    reopen(workspace, window, cx);
                }))
            } else {
                div
            };
            if store.read(cx).active().is_none() {
                return div;
            }
            div.on_action({
                let store = store.clone();
                move |_: &NextStep, _, cx| store.update(cx, |store, cx| store.step_offset(1, cx))
            })
            .on_action({
                let store = store.clone();
                move |_: &PreviousStep, _, cx| {
                    store.update(cx, |store, cx| store.step_offset(-1, cx))
                }
            })
            .on_action({
                let store = store.clone();
                move |_: &Dismiss, _, cx| store.update(cx, |store, cx| store.dismiss(cx))
            })
            .on_action(cx.listener({
                let store = store.clone();
                move |workspace, action: &GoToStep, window, cx| {
                    let step = StepId::new(action.id.clone());
                    let is_active = store
                        .read(cx)
                        .active_step()
                        .is_some_and(|active| active.id == step);
                    if is_active {
                        // No event fires for the step that is already
                        // active, but the user may have scrolled away.
                        reveal_active_step(workspace, &store, window, cx);
                    } else {
                        store.update(cx, |store, cx| store.set_active_step(&step, cx));
                    }
                }
            }))
        }
    });
}

fn reveal_active_step(
    workspace: &mut Workspace,
    store: &Entity<WalkthroughStore>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(step) = store.read(cx).active_step().cloned() else {
        return;
    };
    let pane = code_pane(workspace, &step.path, window, cx);
    let open = workspace.open_path(step.path.clone(), Some(pane.downgrade()), true, window, cx);
    let store = store.downgrade();
    cx.spawn_in(window, async move |_, cx| {
        let item = open.await?;
        let Some(editor) = item.downcast::<Editor>() else {
            return Ok(());
        };
        editor.update_in(cx, |editor, window, cx| {
            let is_still_active = store
                .read_with(cx, |store, _| {
                    store
                        .active_step()
                        .is_some_and(|active| active.id == step.id)
                })
                .unwrap_or(false);
            if !is_still_active {
                return;
            }
            let start = step.point_range(cx).start;
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::top_relative(LINES_ABOVE_STEP)),
                window,
                cx,
                |selections| selections.select_ranges([start..start]),
            );
        })
    })
    .detach_and_log_err(cx);
}

/// Picks the pane to show a step's code in: one that already has the file
/// open, otherwise the pane the user is working in. Never the flowchart's.
fn code_pane(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<Pane> {
    let hosts_flowchart = |pane: &Entity<Pane>, cx: &App| {
        pane.read(cx)
            .items_of_type::<FlowchartView>()
            .next()
            .is_some()
    };
    let code_panes = workspace
        .panes()
        .iter()
        .filter(|pane| !hosts_flowchart(pane, cx))
        .cloned()
        .collect::<Vec<_>>();

    let pane_with_file = code_panes.iter().find(|pane| {
        pane.read(cx).items().any(|item| {
            item.downcast::<Editor>().is_some() && item.project_path(cx).as_ref() == Some(path)
        })
    });
    if let Some(pane) = pane_with_file {
        return pane.clone();
    }
    let active_pane = workspace.active_pane().clone();
    if code_panes.contains(&active_pane) {
        return active_pane;
    }
    if let Some(pane) = code_panes.into_iter().next() {
        return pane;
    }
    workspace.split_pane(active_pane, SplitDirection::Left, window, cx)
}

fn load_from_json(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Load Walkthrough".into()),
    });
    let project = workspace.project().clone();
    cx.spawn_in(window, async move |_, cx| {
        let Some(path) = paths.await??.and_then(|paths| paths.into_iter().next()) else {
            return Ok(());
        };
        load_walkthrough_file(&path, &project, cx).await
    })
    .detach_and_prompt_err("Failed to load walkthrough", window, cx, |_, _, _| None);
}

fn reopen(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let store = WalkthroughStore::for_project(workspace.project(), cx);
    let history_loaded = store.update(cx, |store, cx| store.load_history(cx));
    cx.spawn_in(window, async move |workspace, cx| {
        history_loaded.await;
        workspace.update_in(cx, |workspace, window, cx| {
            let entries = store.read(cx).history().to_vec();
            if entries.is_empty() {
                workspace.show_toast(
                    Toast::new(
                        NotificationId::unique::<Reopen>(),
                        "No previous walkthroughs in this project",
                    )
                    .autohide(),
                    cx,
                );
                return;
            }
            let weak_workspace = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                ReopenPicker::new(entries, weak_workspace, window, cx)
            });
        })
    })
    .detach_and_log_err(cx);
}

pub(crate) fn show_from_history(
    workspace: &mut Workspace,
    json: WalkthroughJson,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    cx.spawn_in(window, async move |_, cx| {
        load_walkthrough_json(json, &project, cx).await
    })
    .detach_and_prompt_err("Failed to reopen walkthrough", window, cx, |_, _, _| None);
}

async fn load_walkthrough_file(
    path: &Path,
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let fs = project.read_with(cx, |project, _| project.fs().clone());
    let text = fs
        .load(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    let json = serde_json::from_str::<WalkthroughJson>(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    load_walkthrough_json(json, project, cx).await
}

/// Validates `json` and makes it the project's active walkthrough, exactly as
/// the `create_walkthrough` agent tool does.
pub(crate) async fn load_walkthrough_json(
    json: WalkthroughJson,
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let load = cx.update(|cx| activate_walkthrough(json, project, cx));
    load.await.map_err(|errors| {
        anyhow!(
            "{}",
            errors
                .iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use language::Point;
    use util::path;
    use workspace::item::ItemHandle as _;

    fn walkthrough_actions_hidden(cx: &mut VisualTestContext) -> bool {
        cx.update(|_, cx| {
            let filter = CommandPaletteFilter::try_global(cx).expect("filter should be set");
            let hidden = [
                filter.is_hidden(&NextStep),
                filter.is_hidden(&PreviousStep),
                filter.is_hidden(&Dismiss),
                filter.is_hidden(&ToggleFlowchart),
                filter.is_hidden(&GoToStep {
                    id: "a".to_string(),
                }),
            ];
            assert!(
                hidden.iter().all(|is_hidden| *is_hidden == hidden[0]),
                "walkthrough actions should be shown or hidden together"
            );
            hidden[0]
        })
    }

    #[gpui::test]
    async fn test_navigation_to_a_file_outside_the_project(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        let mut json = sample_walkthrough(false);
        json.steps[1].path = path!("/other/api/handler.rs").to_string();
        json.steps[1].start_line = 3;
        json.steps[1].end_line = 5;
        let visible_roots = |cx: &mut VisualTestContext| {
            test.project
                .read_with(cx, |project, cx| project.visible_worktrees(cx).count())
        };
        let roots_before = visible_roots(cx);
        load_walkthrough(json, &test, cx).await;
        let one = active_editor(&test, cx);

        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("b"));
        let handler = active_editor(&test, cx);
        assert_ne!(handler, one);
        let opened_path = handler.read_with(cx, |_, cx| {
            let project_path = handler.project_path(cx)?;
            test.project.read(cx).absolute_path(&project_path, cx)
        });
        assert_eq!(
            opened_path.as_deref(),
            Some(Path::new(path!("/other/api/handler.rs")))
        );
        assert_eq!(cursor(&handler, cx), Point::new(2, 0));
        assert_eq!(visible_roots(cx), roots_before);
        let (workspace_panes, handler_pane) = test.workspace.read_with(cx, |workspace, _| {
            (workspace.panes().to_vec(), workspace.pane_for(&handler))
        });
        assert!(handler_pane.is_some_and(|pane| workspace_panes.contains(&pane)));
        assert_eq!(
            handler.read_with(cx, |editor, _| crate::editor_addon::step_block_count(
                editor
            )),
            1
        );

        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("c"));
        assert_eq!(active_editor(&test, cx), one);
    }

    #[gpui::test]
    async fn test_navigation_across_files(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        let three = open_editor("three.rs", &test, cx).await;

        // Without a walkthrough the actions are neither listed nor handled.
        assert!(walkthrough_actions_hidden(cx));
        assert!(!cx.update(|window, cx| window.is_action_available(&NextStep, cx)));
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(active_editor(&test, cx), three);

        load_walkthrough(sample_walkthrough(true), &test, cx).await;
        assert!(!walkthrough_actions_hidden(cx));
        assert!(cx.update(|window, cx| window.is_action_available(&NextStep, cx)));
        let code_pane = test
            .workspace
            .read_with(cx, |workspace, _| workspace.active_pane().clone());

        let pane_of = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            test.workspace
                .read_with(cx, |workspace, _| workspace.pane_for(editor))
                .expect("editor should be in a pane")
        };
        let file_of = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |_, cx| editor.project_path(cx))
        };

        // Creating a walkthrough shows its first step.
        let one = active_editor(&test, cx);
        assert_eq!(file_of(&one, cx), Some(project_path("one.rs", &test, cx)));
        assert_eq!(pane_of(&one, cx), code_pane);
        assert_eq!(cursor(&one, cx), Point::new(1, 0));
        assert!(one.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).is_focused(window)
        }));

        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("b"));
        let two = active_editor(&test, cx);
        assert_eq!(file_of(&two, cx), Some(project_path("two.rs", &test, cx)));
        assert_eq!(pane_of(&two, cx), code_pane);
        assert_eq!(cursor(&two, cx), Point::new(4, 0));
        assert!(two.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).is_focused(window)
        }));

        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("c"));
        assert_eq!(active_editor(&test, cx), one, "the existing tab is reused");
        assert_eq!(cursor(&one, cx), Point::new(9, 0));

        // The last and first steps are the ends.
        cx.dispatch_action(NextStep);
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("c"));
        cx.dispatch_action(PreviousStep);
        cx.dispatch_action(PreviousStep);
        cx.dispatch_action(PreviousStep);
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("a"));
        assert_eq!(active_editor(&test, cx), one);
        assert_eq!(cursor(&one, cx), Point::new(1, 0));

        // Going to the step that is already active brings it back into view.
        test.workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&three, true, true, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(active_editor(&test, cx), three);
        cx.dispatch_action(GoToStep {
            id: "a".to_string(),
        });
        cx.run_until_parked();
        assert_eq!(active_editor(&test, cx), one);

        cx.dispatch_action(GoToStep {
            id: "b".to_string(),
        });
        cx.run_until_parked();
        assert_eq!(active_step(&test, cx).as_deref(), Some("b"));
        assert_eq!(active_editor(&test, cx), two);

        // Code is never opened in the flowchart's pane.
        let flowchart_pane = test.workspace.read_with(cx, |workspace, cx| {
            let view = workspace
                .item_of_type::<FlowchartView>(cx)
                .expect("flowchart should be open");
            workspace.pane_for(&view).expect("flowchart pane")
        });
        assert_ne!(flowchart_pane, code_pane);
        assert_eq!(flowchart_pane.read_with(cx, |pane, _| pane.items_len()), 1);

        cx.dispatch_action(Dismiss);
        cx.run_until_parked();
        assert!(
            test.store
                .read_with(cx, |store, _| store.active().is_none())
        );
        assert!(walkthrough_actions_hidden(cx));
        assert!(!cx.update(|window, cx| window.is_action_available(&NextStep, cx)));
    }

    fn titled_walkthrough(title: &str) -> WalkthroughJson {
        let mut json = sample_walkthrough(false);
        json.title = title.to_string();
        json
    }

    fn reopen_titles(test: &TestWorkspace, cx: &mut VisualTestContext) -> Option<Vec<String>> {
        let picker = test.workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<ReopenPicker>(cx)
                .map(|modal| modal.read(cx).picker().clone())
        })?;
        Some(picker.read_with(cx, |picker, _| {
            picker
                .delegate
                .titles()
                .into_iter()
                .map(str::to_string)
                .collect()
        }))
    }

    #[gpui::test]
    async fn test_reopen_picker_shows_history_and_confirming_reshows_it(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        load_walkthrough(titled_walkthrough("Older"), &test, cx).await;
        load_walkthrough(titled_walkthrough("Newer"), &test, cx).await;

        cx.dispatch_action(Dismiss);
        cx.run_until_parked();
        assert!(
            test.store
                .read_with(cx, |store, _| store.active().is_none())
        );

        cx.dispatch_action(Reopen);
        cx.run_until_parked();
        assert_eq!(
            reopen_titles(&test, cx),
            Some(vec!["Newer".to_string(), "Older".to_string()])
        );

        let picker = test.workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_modal::<ReopenPicker>(cx)
                .map(|modal| modal.read(cx).picker().clone())
                .expect("the picker should be open")
        });
        picker.update_in(cx, |picker, window, cx| {
            picker.set_selected_index(1, None, true, window, cx);
        });
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();

        assert!(reopen_titles(&test, cx).is_none());
        let title = test.store.read_with(cx, |store, _| {
            store
                .active()
                .map(|walkthrough| walkthrough.title.to_string())
        });
        assert_eq!(title.as_deref(), Some("Older"));
        let history = test.store.read_with(cx, |store, _| {
            store
                .history()
                .iter()
                .map(|entry| entry.walkthrough.title.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(history, ["Older", "Newer"]);
    }

    #[gpui::test]
    async fn test_reopen_without_history_opens_no_picker(cx: &mut TestAppContext) {
        let (test, cx) = init_test(cx).await;
        cx.dispatch_action(Reopen);
        cx.run_until_parked();
        assert!(reopen_titles(&test, cx).is_none());
        assert!(
            test.store
                .read_with(cx, |store, _| store.active().is_none())
        );
    }
}
