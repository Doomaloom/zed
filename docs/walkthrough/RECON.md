# Code walkthroughs: recon

This resolves every _(verify)_ in the technical spec against the checked-out code. Where this file and the spec disagree, this file wins. Line numbers are as of the commit that added this file and will drift; the names are what matter.

## Deviations from the spec

| Spec says | Reality | What was done |
| --- | --- | --- |
| Mermaid is rendered by a `mermaid-rs-renderer` fork | The renderer is `merman`, wrapped by `crates/mermaid_render` | Call `mermaid_render::render_to_svg` directly |
| `MermaidState` and `CachedMermaidDiagram` in `crates/markdown/src/mermaid.rs` can be reused | Both are private (`pub(crate)` and private) and tied to a `Markdown` entity | `FlowchartView` has its own small cache keyed by source and calls the renderer itself (the spec's first choice) |
| `editor.highlight_background::<Marker>(…)` keyed by a marker type | Highlights are keyed by the `HighlightKey` enum | Added `HighlightKey::WalkthroughStep` and `HighlightKey::WalkthroughActiveStep` (the only change inside `crates/editor`) |
| Block `height` may need estimating and `resize_blocks` on width changes | `height` is `Option<u32>` and the editor measures every custom block on layout, resizing it to fit | Blocks start at 2 rows and the editor corrects them; no manual resizing |
| `AgentTool::name()` returns the tool name | The name is `const NAME: &'static str`; `kind()` and `description()` are associated functions | Implemented as the trait defines |
| `parse_check(mermaid: &str) -> Result<(), String>` | Rendering needs an `SvgRenderer` to measure text | `parse_check(mermaid, &SvgRenderer)`; `flowchart_view::init` registers it as `walkthrough::GlobalFlowchartCheck` so `agent` can reach it without depending on a UI crate |
| WP1 adds the `LoadFromJson` action handler in `crates/walkthrough` | Registering a workspace action needs `workspace`, which `walkthrough` must not depend on | The action _types_ live in `walkthrough`; the handler is in `walkthrough_ui/src/navigation.rs` |
| Default keys `alt-]` and `alt-[` | Taken on Linux and Windows by `editor::NextEditPrediction` and `editor::PreviousEditPrediction` | `ctrl-alt-]` and `ctrl-alt-[` on all three platforms |
| A visual test using the runner in `crates/zed` | `zed-visual-test-runner` is macOS-only (`#[cfg(target_os = "macos")]`, Metal) | Not added; it could not be compiled or run on the Linux machine this was built on |
| Branch `walkthrough` | The work was done on the session's assigned branch | Branch `claude/implementation-spec-cg0zc2` |

## Agent tool

- **Trait:** `AgentTool` in `crates/agent/src/thread.rs`. Associated items: `type Input`, `type Output`, `const NAME`, `fn kind() -> acp::ToolKind`, `fn initial_title(&self, Result<Input, serde_json::Value>, &mut App) -> SharedString`, and `fn run(self: Arc<Self>, ToolInput<Input>, ToolCallEventStream, &mut App) -> Task<Result<Output, Output>>`. The description is read from the doc comment on `Input` through its JSON schema.
- **Errors** are returned as `Err(Output)`, not `anyhow::Error`, so `Output` is an untagged `Success`/`Error` enum. Copy `crates/agent/src/tools/find_path_tool.rs`.
- **Registration**, all five are needed or the tool is silently dropped:
  1. `mod` + `pub use` + the `tools!` list in `crates/agent/src/tools.rs`.
  2. `Thread::add_default_tools` in `crates/agent/src/thread.rs`.
  3. The `write` and `ask` profiles in `assets/settings/default.json` (`"create_walkthrough": true`).
  4. `agent_profile_tools_schema` in `crates/settings_content/src/agent.rs`.
  5. `EXCLUDED_TOOLS` in `crates/settings_ui/src/pages/tool_permissions_setup.rs` (the tool never prompts).
- **Gating:** `tool_feature_flag_enabled` in `crates/agent/src/tools.rs` maps a tool name to a feature flag. See Feature flag below.
- **Tests:** `ToolInput::resolved(input)` and `ToolCallEventStream::test()`; call site in `find_path_tool.rs`.

## Editor blocks

- `BlockProperties<P> { placement: BlockPlacement<P>, height: Option<u32>, style: BlockStyle, render: RenderBlock, priority: usize }` in `crates/editor/src/display_map/block_map.rs`.
- `BlockPlacement::{Above, Below, Near, Replace}`; `BlockStyle::{Fixed, Flex, Spacer, Sticky}`.
- `RenderBlock = Arc<dyn Send + Sync + Fn(&mut BlockContext) -> AnyElement>`. `BlockContext` has `window`, `app`, `margins`, `em_width`, `line_height`, `max_width`, `block_id`.
- On `Editor`: `insert_blocks(blocks, autoscroll: Option<Autoscroll>, cx) -> Vec<CustomBlockId>`, `remove_blocks(HashSet<CustomBlockId>, autoscroll, cx)`, `replace_blocks(HashMap<CustomBlockId, RenderBlock>, autoscroll, cx)`, `resize_blocks(HashMap<CustomBlockId, u32>, autoscroll, cx)`.
- **Height is measured automatically:** `EditorElement::render_block` in `crates/editor/src/element.rs` lays the element out, rounds its height up to whole lines and resizes the block if it differs.
- A `Sticky` block spans the gutter, so content is padded by `block_context.margins.gutter.full_width()`.
- Call sites to copy: the diff review overlay (`show_diff_review_overlay` in `crates/editor/src/git.rs`) and `crates/diagnostics/src/diagnostic_renderer.rs`, which renders markdown inside a block from outside the editor crate.

## Background highlights

- `Editor::highlight_background(key: HighlightKey, ranges: &[Range<Anchor>], color_fetcher: impl Fn(&usize, &Theme) -> Hsla + Send + Sync + 'static, cx)`.
- `Editor::clear_background_highlights(key, cx)`.
- Ranges should be sorted by position. A buffer range is lifted to editor anchors with `MultiBufferSnapshot::anchor_range_in_buffer`.
- Colours used: `editor_document_highlight_read_background` for inactive steps and `text_accent` at 18% opacity for the active step. No new theme keys.

## Observing new editors and per-editor state

- `cx.observe_new(|editor: &mut Editor, window: Option<&mut Window>, cx: &mut Context<Editor>| …)`; call site `crates/git_ui/src/git_ui.rs` (`conflict_view::register_editor`).
- Per-editor state goes in an `editor::Addon` (`register_addon`, `addon::<T>()`, `addon_mut::<T>()`; implement `to_any_mut`).
- Skip editors that are not `editor.mode().is_full()` or not `buffer().read(cx).is_singleton()`.

## Opening a path and scrolling

- `Workspace::open_path(path: impl Into<ProjectPath>, pane: Option<WeakEntity<Pane>>, focus_item: bool, window, cx) -> Task<Result<Box<dyn ItemHandle>>>`. With a pane it reuses that pane's existing tab for the path.
- `Editor::change_selections(SelectionEffects::scroll(Autoscroll::…), window, cx, |selections| selections.select_ranges([point..point]))`.
- `Autoscroll::top_relative(lines)` puts the cursor a fixed number of lines from the top, which shows the range first and leaves room for the comment block below it. `Autoscroll::fit()` and `Autoscroll::center()` also exist.
- Splits: `Workspace::adjacent_pane(window, cx)` finds or creates the pane to the right. Creating a pane focuses it, so the previous focus has to be restored.

## Conditional actions

- `Workspace::register_action_renderer(|div, workspace, window, cx| …)` adds handlers at render time, so an action can be available only in some states. Unhandled actions do not swallow their key bindings.
- `CommandPaletteFilter::update_global(cx, |filter, _| filter.hide_action_types(&[TypeId::of::<A>()]))` hides actions from the palette. The filter is global, not per window.

## Markdown

- `markdown::Markdown::new(source: SharedString, language_registry: Option<Arc<LanguageRegistry>>, fallback_code_block_language, cx)` is the entity; `MarkdownElement::new(entity, MarkdownStyle::themed(MarkdownFont::Editor, window, cx))` is the element. `MarkdownFont::Agent` matches the Agent panel.

## Mermaid

- Entry point: `mermaid_render::render_to_svg(source: &str, theme: &MermaidTheme, svg_renderer: &SvgRenderer) -> anyhow::Result<String>`.
- Rasterizing: `cx.svg_renderer()`, then `SvgRenderer::parse_svg(bytes)` and `render_parsed(&parsed, scale) -> Arc<RenderImage>`, painted with `img(ImageSource::Render(image))`. Images are released with `cx.drop_image`.
- `markdown`'s theme builder is private, so `flowchart_view.rs` has its own `mermaid_theme`.
- **`style` directives work.** `style verify fill:#ff0000,stroke:#00ff00,stroke-width:2px` comes out as an inline `style` attribute on the node's shape, and none of the `!important` rules Zed injects target flowchart nodes.
- **Node rectangles are available from the SVG.** Each node is `<g class="node default" id="<diagram>-flowchart-<node id>-<n>" transform="translate(cx,cy)">` whose first shape child (`rect`, `polygon`, `circle`, `ellipse`) gives its extent. Nodes in subgraphs sit under further translated groups. The root `<svg>` has a `viewBox`. This is the second route in the spec; no renderer change was needed.
- Errors: `error.root_cause()` is the useful text, for example `Diagram parse error (flowchart-v2): Unexpected character at 23`.

## Per-project state

- Existing pattern: a `gpui::Global` holding `HashMap<WeakEntity<Project>, …>` (`crates/search/src/project_search.rs`, `crates/debugger_tools/src/dap_log.rs`).
- `WalkthroughStore::for_project` does the same and removes the entry with `cx.observe_release`.

## Feature flag

- `feature_flags::WalkthroughFeatureFlag`, name `walkthrough`, in `crates/feature_flags/src/flags.rs`. Pattern copied from `CreateThreadToolFeatureFlag`.
- Staff flags are on in debug builds, so a locally built fork has the feature. Elsewhere it is off unless `"feature_flags": { "walkthrough": "on" }` is in `settings.json`. `ZED_DISABLE_STAFF=1` turns it off in a debug build.
- It gates the tool (`tool_feature_flag_enabled`), `walkthrough: load from json` and `walkthrough: toggle flowchart`.

## Keymaps

- `assets/keymaps/default-{macos,linux,windows}.json`, in the `"context": "Workspace"` block.
- `ctrl-alt-]` and `ctrl-alt-[` are unused in all three default keymaps. On keyboard layouts that type `[` and `]` with AltGr these may overlap with typing; the bindings only apply while a walkthrough is active.

## Build notes

- A debug build with debug info needs more than 40 GB of disk. `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0` keeps it under 20 GB.
- Three `agent` tests (`test_fetch_tool_*redirect*`) need DNS and fail in a sandbox without it. They are unrelated to this feature.
