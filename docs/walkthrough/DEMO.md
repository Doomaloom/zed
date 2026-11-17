# Code walkthroughs: demo

A code walkthrough is an ordered set of agent-written comments pinned to highlighted ranges in normal editor buffers, with a flowchart beside the code. The Zed Agent, and external agents through Zed's `zed` MCP server, create one with the `create_walkthrough` tool. Nothing is written to the files.

This is a prototype. The automated tests cover state and structure; how it looks has not been checked by a person yet. The checklist below is for that.

## Turning it on

The `walkthrough` feature flag is enabled for all users in this fork, so the feature is on in every build.

The Zed Agent has the tool in the default Write and Ask profiles. External agents (Copilot, Claude Agent, Codex, Gemini CLI and other ACP agents) get the same tool from an MCP server named `zed` that Zed adds to their sessions automatically. This works on Linux and macOS, for local projects.

## Try it without an agent

1. Open the Zed repository in the built fork.
2. Run `walkthrough: load from json` from the command palette.
3. Pick `crates/walkthrough/test_fixtures/sample.json`.

This loads a nine-step walkthrough of this feature's own code, through the same validation the tool uses.

## Try it with the agent

Open the Agent panel with the Zed Agent and ask:

> Walk me through how the diff review overlay stores and sends comments.

What should happen:

1. The agent reads the relevant files, then calls `create_walkthrough` once.
2. The file for step 1 opens and scrolls to its range. The range is highlighted and a comment block sits under it showing "1 / N", the step title and the explanation.
3. A flowchart tab opens in a split to the right, with the node for step 1 highlighted and a step list under the diagram.
4. The agent replies in chat with a sentence or two and does not repeat the explanation.

The same prompt works in an external agent thread. There the call shows up as the `create_walkthrough` tool of the `zed` server, and the agent may ask for permission to use it.

## Actions

| Action | Default key | What it does |
| --- | --- | --- |
| `walkthrough: next step` | `ctrl-alt-]` | Moves to the next step, opening its file if needed |
| `walkthrough: previous step` | `ctrl-alt-[` | Moves to the previous step |
| `walkthrough: toggle flowchart` | | Opens or closes the flowchart tab |
| `walkthrough: dismiss` | | Removes all highlights, comment blocks and the flowchart tab |
| `walkthrough: load from json` | | Loads a walkthrough from a JSON file |

All except the last are available only while a walkthrough is active. Each comment block also has previous, next and collapse buttons, and clicking a step title, a row in the step list or a flowchart node jumps to that step.

## Checklist for a person

- [ ] The agent prompt above produces one `create_walkthrough` call and a brief chat reply.
- [ ] Step 1's file opens, the range is highlighted, and the comment block is readable, correctly sized, and wraps at the editor width.
- [ ] A long comment scrolls inside its block instead of growing past about 14 lines.
- [ ] The active step's highlight and left border are clearly stronger than the others.
- [ ] Coloured steps and inner highlights use the colours named in the walkthrough, inner highlights are stronger in the active step, and each comment block lists the legend labels for the colours it uses.
- [ ] The flowchart panel shows the full legend above the step list and a dot beside each coloured step.
- [ ] A walkthrough with no colours looks the same as before.
- [ ] Next and previous move across at least two files, and the flowchart highlight follows.
- [ ] The flowchart opens to the right and focus stays in the code editor.
- [ ] Clicking a step row jumps to its code. Clicking a flowchart node does the same, and the pointer changes over clickable nodes.
- [ ] The flowchart is legible at the pane's width, and shrinks when the pane is narrower than the diagram.
- [ ] Typing above an annotated range moves the highlight with the code. Deleting an annotated range shows "This code has changed since the walkthrough was written" and removes the highlight.
- [ ] Collapsing a comment block leaves a single row with the counter and title.
- [ ] Folding, multiple cursors, inline diagnostics, git gutter marks and the minimap still behave normally in an annotated file.
- [ ] Scrolling a 5,000-line file with 12 steps stays smooth.
- [ ] Dismiss removes everything, and the files show no modifications.
- [ ] Creating a second walkthrough replaces the first cleanly.
- [ ] Light and dark themes are both readable, in the editor and in the flowchart, including all six highlight colours.
- [ ] `ctrl-alt-]` and `ctrl-alt-[` do not get in the way of typing on your keyboard layout.

## Known gaps

- The full `zed` binary was not built or launched during development (missing ALSA headers on the build machine). Confirm `cargo run` starts before working through the checklist.
- No visual regression test. The runner in `crates/zed` is macOS-only and this was built on Linux.
- Not available on Windows. The `zed` MCP server needs Unix sockets.
- Not available for remote (SSH) projects. The `zed` MCP server is added for local projects only.
- Not added if you already have a context server named `zed`. Rename yours to get the walkthrough tool.
- With ACP v2 agents, the server is only added if the agent advertises stdio MCP support.
- Not verified against each external agent by a person yet. The MCP server's tests were written but not run, and the bridge (`zed --nc <socket>`) has not been exercised end to end.
- Multibuffer editors (project search, diagnostics, diffs) are not annotated.
- A walkthrough lives in memory: it is gone after a restart and is not shared with collaborators.
- The flowchart has no pan or zoom.
- If the same file is open in two panes, both are annotated but navigation uses the first pane it finds.

## Changes inside `crates/editor`

For the upstream proposal: the only change is three variants added to `HighlightKey` in `crates/editor/src/display_map.rs` (`WalkthroughStep`, `WalkthroughActiveStep`, `WalkthroughHighlight`). Everything else uses the editor's public API from `crates/walkthrough_ui`.
