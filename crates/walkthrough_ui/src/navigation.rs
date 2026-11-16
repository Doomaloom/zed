use std::{any::TypeId, path::Path};

use anyhow::{Context as _, Result, anyhow};
use command_palette_hooks::CommandPaletteFilter;
use editor::{Editor, SelectionEffects, scroll::Autoscroll};
use feature_flags::{FeatureFlagAppExt as _, WalkthroughFeatureFlag};
use gpui::{App, AsyncApp, Context, Entity, PathPromptOptions, Window, prelude::*};
use project::{Project, ProjectPath};
use walkthrough::{
    Dismiss, GlobalFlowchartCheck, GoToStep, LoadFromJson, NextStep, PreviousStep, StepId,
    ToggleFlowchart, Walkthrough, WalkthroughEvent, WalkthroughJson, WalkthroughStore,
};
use workspace::{
    Pane, SplitDirection, Workspace, notifications::DetachAndPromptErr as _,
};

use crate::FlowchartView;

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
    let load_action = [TypeId::of::<LoadFromJson>()];
    let enabled = cx.has_flag::<WalkthroughFeatureFlag>();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if has_walkthrough {
            filter.show_action_types(&walkthrough_actions);
        } else {
            filter.hide_action_types(&walkthrough_actions);
        }
        if enabled {
            filter.show_action_types(&load_action);
        } else {
            filter.hide_action_types(&load_action);
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
                WalkthroughEvent::Dismissed => {}
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
    let load = cx.update(|cx| {
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
    });
    let walkthrough = load.await.map_err(|errors| {
        anyhow!(
            "{}",
            errors
                .iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    cx.update(|cx| {
        WalkthroughStore::for_project(project, cx)
            .update(cx, |store, cx| store.set_walkthrough(walkthrough, cx));
    });
    Ok(())
}
