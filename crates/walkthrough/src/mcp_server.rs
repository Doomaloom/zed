use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use context_server::listener::{McpServer, McpServerTool, ToolResponse, tool_definition};
use context_server::types::{
    Implementation, InitializeParams, InitializeResponse, LATEST_PROTOCOL_VERSION,
    ListToolsResponse, ProtocolVersion, ServerCapabilities, ToolAnnotations, ToolResponseContent,
    ToolsCapabilities, VERSION_2024_11_05, VERSION_2025_03_26, VERSION_2025_06_18, requests,
};
use feature_flags::{FeatureFlagAppExt as _, WalkthroughFeatureFlag};
use gpui::{App, AsyncApp, Entity, Global, Subscription, Task, WeakEntity};
use project::{Event as ProjectEvent, Project};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use util::ResultExt as _;

pub const ZED_MCP_SERVER_NAME: &str = "zed";

const NOT_RUNNING_MESSAGE: &str =
    "Zed is not running with a local project open. Open the project in Zed and try again.";

/// The stdio command an external agent runs to reach Zed's in-process MCP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZedMcpServerCommand {
    pub path: PathBuf,
    pub args: Vec<String>,
}

#[derive(Clone)]
struct CreateWalkthroughMcpTool {
    project: WeakEntity<Project>,
}

impl McpServerTool for CreateWalkthroughMcpTool {
    type Input = crate::WalkthroughJson;
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
        let shown = cx.update(|cx| crate::show_walkthrough(input, &project, cx));
        match shown.await {
            Ok(message) => Ok(ToolResponse {
                content: vec![ToolResponseContent::Text { text: message }],
                structured_content: (),
            }),
            Err(error) => Err(anyhow!(error)),
        }
    }
}

/// What a standalone `zed --mcp` process needs to know about a running server.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryEntry {
    socket: PathBuf,
    roots: Vec<PathBuf>,
}

fn default_registry_directory() -> PathBuf {
    paths::data_dir().join("mcp_servers")
}

struct RegistryDirectoryOverride(PathBuf);

impl Global for RegistryDirectoryOverride {}

/// Changes where servers announce themselves, instead of the default
/// `mcp_servers` folder in Zed's data directory. Tests use this so they do
/// not write into the real data directory.
pub fn set_registry_directory(directory: PathBuf, cx: &mut App) {
    cx.set_global(RegistryDirectoryOverride(directory));
}

fn registry_directory(cx: &App) -> PathBuf {
    cx.try_global::<RegistryDirectoryOverride>()
        .map_or_else(default_registry_directory, |directory| directory.0.clone())
}

/// The registry file of one server. It is removed when dropped.
struct RegistryPublisher {
    path: PathBuf,
    socket: PathBuf,
}

impl RegistryPublisher {
    fn new(directory: &Path, socket: PathBuf) -> Self {
        static NEXT_SERVER_NUMBER: AtomicUsize = AtomicUsize::new(0);
        let file_name = format!(
            "{}-{}.json",
            std::process::id(),
            NEXT_SERVER_NUMBER.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            path: directory.join(file_name),
            socket,
        }
    }

    fn publish(&self, project: &Project, cx: &App) {
        let entry = RegistryEntry {
            socket: self.socket.clone(),
            roots: project
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                .collect(),
        };
        write_registry_entry(&self.path, &entry).log_err();
    }
}

impl Drop for RegistryPublisher {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "removing MCP server registry file {}: {error}",
                self.path.display()
            );
        }
    }
}

fn write_registry_entry(path: &Path, entry: &RegistryEntry) -> Result<()> {
    let directory = path.parent().context("registry file has no directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    // Readers must never see a half-written file.
    let temporary_path = path.with_extension("tmp");
    std::fs::write(&temporary_path, serde_json::to_vec(entry)?)
        .with_context(|| format!("writing {}", temporary_path.display()))?;
    std::fs::rename(&temporary_path, path).with_context(|| format!("replacing {}", path.display()))
}

struct ProjectServer {
    command: ZedMcpServerCommand,
    // Dropping the server stops its socket listener, so it is kept for as
    // long as the project is open.
    _server: McpServer,
    _registry: Rc<RegistryPublisher>,
    _worktree_subscription: Subscription,
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
pub fn zed_mcp_server_for_project(
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
    let registry = Rc::new(RegistryPublisher::new(
        &registry_directory(cx),
        server.socket_path().to_path_buf(),
    ));
    registry.publish(project.read(cx), cx);
    let worktree_subscription = cx.subscribe(project, {
        let registry = registry.clone();
        move |project, event, cx| {
            if matches!(
                event,
                ProjectEvent::WorktreeAdded(_)
                    | ProjectEvent::WorktreeRemoved(_)
                    | ProjectEvent::WorktreeOrderChanged
                    | ProjectEvent::WorktreePathsChanged { .. }
            ) {
                registry.publish(project.read(cx), cx);
            }
        }
    });
    cx.default_global::<GlobalZedMcpServers>().0.insert(
        weak_project.clone(),
        ProjectServer {
            command: command.clone(),
            _server: server,
            _registry: registry,
            _worktree_subscription: worktree_subscription,
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

fn handle_initialize(params: InitializeParams, _cx: &App) -> Task<Result<InitializeResponse>> {
    Task::ready(Ok(initialize_response(params.protocol_version.0)))
}

fn initialize_response(requested_protocol_version: String) -> InitializeResponse {
    let protocol_version = if matches!(
        requested_protocol_version.as_str(),
        VERSION_2024_11_05 | VERSION_2025_03_26 | VERSION_2025_06_18 | LATEST_PROTOCOL_VERSION
    ) {
        requested_protocol_version
    } else {
        LATEST_PROTOCOL_VERSION.to_string()
    };
    InitializeResponse {
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
    }
}

/// Runs the MCP server that `zed --mcp` provides to clients which were not
/// started by Zed. It speaks newline-delimited JSON-RPC on standard input and
/// output, and forwards tool calls to a running Zed.
pub fn run_stdio_server() -> Result<()> {
    let current_directory = std::env::current_dir().log_err();
    serve_stdio(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &default_registry_directory(),
        current_directory.as_deref(),
    )
}

fn serve_stdio(
    input: impl BufRead,
    mut output: impl Write,
    registry_directory: &Path,
    current_directory: Option<&Path>,
) -> Result<()> {
    for line in input.lines() {
        let line = line.context("reading from standard input")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = respond_to_message(&line, registry_directory, current_directory) {
            writeln!(output, "{response}").context("writing to standard output")?;
            output.flush().context("writing to standard output")?;
        }
    }
    Ok(())
}

fn respond_to_message(
    line: &str,
    registry_directory: &Path,
    current_directory: Option<&Path>,
) -> Option<String> {
    let message = match serde_json::from_str::<Value>(line) {
        Ok(message) => message,
        Err(error) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("Failed to parse: {error}"),
            ));
        }
    };
    let id = message.get("id").filter(|id| !id.is_null())?.clone();
    let method = message.get("method")?.as_str()?;

    let result = match method {
        "initialize" => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            match serde_json::from_value::<InitializeParams>(params) {
                Ok(params) => serde_json::to_value(initialize_response(params.protocol_version.0)),
                Err(error) => {
                    return Some(error_response(id, -32602, &error.to_string()));
                }
            }
        }
        "tools/list" => serde_json::to_value(ListToolsResponse {
            tools: vec![tool_definition(&CreateWalkthroughMcpTool {
                project: WeakEntity::new_invalid(),
            })],
            next_cursor: None,
            meta: None,
        }),
        "ping" => Ok(json!({})),
        "tools/call" => {
            return Some(forward_tool_call(
                line,
                &id,
                registry_directory,
                current_directory,
            ));
        }
        _ => {
            return Some(error_response(
                id,
                -32601,
                &format!("unhandled method {method}"),
            ));
        }
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
        Err(error) => error_response(id, -32603, &error.to_string()),
    })
}

fn error_response(id: Value, code: i32, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn tool_error_response(id: &Value, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": message }], "isError": true }
    })
    .to_string()
}

fn forward_tool_call(
    request: &str,
    id: &Value,
    registry_directory: &Path,
    current_directory: Option<&Path>,
) -> String {
    for candidate in ordered_candidates(read_registry(registry_directory), current_directory) {
        let (reader, mut writer) = match nc::connect(&candidate.entry.socket) {
            Ok(halves) => halves,
            Err(error) => {
                // The Zed that wrote this file is gone.
                eprintln!(
                    "Removing stale MCP server registration {}: {error:#}",
                    candidate.path.display()
                );
                std::fs::remove_file(&candidate.path).log_err();
                continue;
            }
        };
        let forwarded = writeln!(writer, "{request}")
            .and_then(|()| writer.flush())
            .context("sending the request to Zed")
            .and_then(|()| {
                let mut response = String::new();
                let bytes_read = BufReader::new(reader)
                    .read_line(&mut response)
                    .context("reading the response from Zed")?;
                anyhow::ensure!(bytes_read > 0, "Zed closed the connection");
                Ok(response)
            });
        return match forwarded {
            Ok(response) => response.trim_end().to_string(),
            Err(error) => tool_error_response(id, &format!("{error:#}")),
        };
    }
    tool_error_response(id, NOT_RUNNING_MESSAGE)
}

#[derive(Debug)]
struct RegistryCandidate {
    path: PathBuf,
    entry: RegistryEntry,
    modified: SystemTime,
}

fn read_registry(directory: &Path) -> Vec<RegistryCandidate> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(|directory_entry| {
            let path = directory_entry.ok()?.path();
            if path.extension()? != "json" {
                return None;
            }
            let modified = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()?;
            let entry = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            Some(RegistryCandidate {
                path,
                entry,
                modified,
            })
        })
        .collect()
}

/// Orders servers by how well they match the client's working directory: the
/// server with the deepest root containing it first, then the others, with the
/// most recently registered first among equals.
fn ordered_candidates(
    mut candidates: Vec<RegistryCandidate>,
    current_directory: Option<&Path>,
) -> Vec<RegistryCandidate> {
    let mut directories = Vec::from_iter(current_directory.map(Path::to_path_buf));
    directories.extend(current_directory.and_then(|path| path.canonicalize().ok()));
    let containing_depth = |candidate: &RegistryCandidate| {
        candidate
            .entry
            .roots
            .iter()
            .filter(|root| {
                directories
                    .iter()
                    .any(|directory| directory.starts_with(root))
            })
            .map(|root| root.components().count())
            .max()
    };
    candidates.sort_by_key(|candidate| {
        std::cmp::Reverse((containing_depth(candidate), candidate.modified))
    });
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    struct RegistryDirectoryGuard(#[allow(dead_code)] tempfile::TempDir);

    impl Global for RegistryDirectoryGuard {}

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            let directory = tempfile::tempdir().expect("creating a registry directory");
            set_registry_directory(directory.path().to_path_buf(), cx);
            cx.set_global(RegistryDirectoryGuard(directory));
        })
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

    fn tool_input(path: &str) -> crate::WalkthroughJson {
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
        let store = cx.update(|cx| crate::WalkthroughStore::for_project(&project, cx));
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
        let store = cx.update(|cx| crate::WalkthroughStore::for_project(&project, cx));
        store.read_with(cx, |store, _| {
            assert!(store.active().is_none());
        });
    }

    #[gpui::test]
    async fn test_server_lists_tool_over_socket(cx: &mut TestAppContext) {
        use futures::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, io::BufReader};

        cx.executor().allow_parking();
        let project = test_project(cx).await;
        let command = cx
            .update(|cx| zed_mcp_server_for_project(&project, cx))
            .expect("a server command");
        let socket_path = command.args.last().expect("a socket path");

        let stream = net::async_net::UnixStream::connect(socket_path)
            .await
            .expect("connecting to the server socket");
        let (reader, mut writer) = stream.split();
        let mut reader = BufReader::new(reader);
        let mut request = async |message: serde_json::Value| {
            writer
                .write_all(format!("{message}\n").as_bytes())
                .await
                .expect("writing a request");
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("reading a response");
            serde_json::from_str::<serde_json::Value>(&line).expect("a JSON response")
        };

        let initialized = request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }
        }))
        .await;
        assert_eq!(
            initialized["result"]["serverInfo"]["name"], "zed",
            "unexpected initialize response: {initialized}"
        );

        let tools = request(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        assert_eq!(
            tools["result"]["tools"][0]["name"], "create_walkthrough",
            "unexpected tools/list response: {tools}"
        );
        assert!(tools["result"]["tools"][0]["description"].is_string());
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

    fn request_line(message: serde_json::Value) -> String {
        format!("{message}\n")
    }

    fn stdio_responses(
        requests: &[serde_json::Value],
        registry_directory: &Path,
        current_directory: Option<&Path>,
    ) -> Vec<serde_json::Value> {
        let input = requests
            .iter()
            .cloned()
            .map(request_line)
            .collect::<String>();
        let mut output = Vec::new();
        serve_stdio(
            input.as_bytes(),
            &mut output,
            registry_directory,
            current_directory,
        )
        .expect("serving stdio");
        String::from_utf8(output)
            .expect("UTF-8 output")
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON response per line"))
            .collect()
    }

    #[test]
    fn test_stdio_server_answers_locally_without_zed_running() {
        let registry = tempfile::tempdir().expect("a registry directory");
        let responses = stdio_responses(
            &[
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": { "name": "test", "version": "0" }
                    }
                }),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                json!({ "jsonrpc": "2.0", "id": "two", "method": "tools/list" }),
                json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }),
                json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "tools/call",
                    "params": { "name": "create_walkthrough", "arguments": {} }
                }),
            ],
            registry.path(),
            None,
        );

        let [initialized, tools, ping, unknown, call] = responses.as_slice() else {
            panic!("unexpected responses: {responses:?}");
        };
        assert_eq!(initialized["result"]["serverInfo"]["name"], "zed");
        assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(tools["id"], "two");
        assert_eq!(tools["result"]["tools"][0]["name"], "create_walkthrough");
        assert!(tools["result"]["tools"][0]["description"].is_string());
        assert!(tools["result"]["tools"][0]["inputSchema"]["properties"]["steps"].is_object());
        assert_eq!(ping["result"], json!({}));
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(call["id"], 5);
        assert_eq!(call["result"]["isError"], true);
        assert_eq!(call["result"]["content"][0]["text"], NOT_RUNNING_MESSAGE);
    }

    #[test]
    fn test_stdio_server_reports_unparseable_input() {
        let registry = tempfile::tempdir().expect("a registry directory");
        let mut output = Vec::new();
        serve_stdio("not json\n".as_bytes(), &mut output, registry.path(), None)
            .expect("serving stdio");
        let response: serde_json::Value = serde_json::from_slice(&output).expect("a JSON response");
        assert_eq!(response["error"]["code"], -32700);
        assert!(response["id"].is_null());
    }

    #[cfg(unix)]
    #[test]
    fn test_stdio_server_forwards_tool_calls_and_removes_stale_registrations() {
        use std::io::Read as _;
        use std::os::unix::net::UnixListener;

        let registry = tempfile::tempdir().expect("a registry directory");
        let sockets = tempfile::tempdir().expect("a socket directory");
        let live_socket = sockets.path().join("live.sock");
        let listener = UnixListener::bind(&live_socket).expect("binding a socket");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accepting a connection");
            let mut request = String::new();
            BufReader::new(&stream)
                .read_line(&mut request)
                .expect("reading the forwarded request");
            let mut writer = &stream;
            writer
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"forwarded\":true}}\n")
                .expect("writing a response");
            let mut rest = Vec::new();
            (&stream).read_to_end(&mut rest).ok();
            request
        });

        let stale_path = registry.path().join("stale.json");
        write_registry_entry(
            &stale_path,
            &RegistryEntry {
                socket: sockets.path().join("gone.sock"),
                roots: vec![PathBuf::from("/work")],
            },
        )
        .expect("writing a stale entry");
        write_registry_entry(
            &registry.path().join("live.json"),
            &RegistryEntry {
                socket: live_socket,
                roots: vec![PathBuf::from("/elsewhere")],
            },
        )
        .expect("writing a live entry");

        let call = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "create_walkthrough", "arguments": {} }
        });
        let responses = stdio_responses(
            std::slice::from_ref(&call),
            registry.path(),
            Some(Path::new("/work/project")),
        );

        assert_eq!(
            responses,
            [json!({"jsonrpc":"2.0","id":7,"result":{"forwarded":true}})]
        );
        assert!(!stale_path.exists());
        let forwarded = server.join().expect("server thread");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&forwarded).expect("forwarded JSON"),
            call
        );
    }

    #[test]
    fn test_candidates_prefer_the_deepest_root_containing_the_current_directory() {
        let candidate = |name: &str, roots: &[&str], age_seconds: u64| RegistryCandidate {
            path: PathBuf::from(name),
            entry: RegistryEntry {
                socket: PathBuf::from(name),
                roots: roots.iter().map(PathBuf::from).collect(),
            },
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 - age_seconds),
        };
        let candidates = vec![
            candidate("newest-unrelated", &["/other"], 0),
            candidate("outer", &["/work"], 10),
            candidate("inner", &["/home", "/work/project"], 20),
            candidate("old-unrelated", &["/third"], 30),
        ];

        let names = |ordered: Vec<RegistryCandidate>| {
            ordered
                .into_iter()
                .map(|candidate| candidate.path.display().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(ordered_candidates(
                candidates,
                Some(Path::new("/work/project/src"))
            )),
            ["inner", "outer", "newest-unrelated", "old-unrelated"]
        );

        let candidates = vec![
            candidate("older", &["/a"], 5),
            candidate("newer", &["/b"], 1),
        ];
        assert_eq!(
            names(ordered_candidates(candidates, Some(Path::new("/nowhere")))),
            ["newer", "older"]
        );
    }

    #[gpui::test]
    async fn test_server_registers_itself_and_unregisters_on_release(cx: &mut TestAppContext) {
        let project = test_project(cx).await;
        let directory = cx.update(|cx| registry_directory(cx));
        let command = cx
            .update(|cx| zed_mcp_server_for_project(&project, cx))
            .expect("a server command");

        let candidates = read_registry(&directory);
        let [candidate] = candidates.as_slice() else {
            panic!("expected one registry file, got {candidates:?}");
        };
        assert_eq!(
            candidate.entry.socket,
            PathBuf::from(command.args.last().expect("a socket path"))
        );
        assert_eq!(candidate.entry.roots, [PathBuf::from(path!("/root"))]);

        drop(project);
        cx.run_until_parked();
        assert!(read_registry(&directory).is_empty());
    }

    #[gpui::test]
    async fn test_registration_follows_worktree_changes(cx: &mut TestAppContext) {
        init_test(cx);
        let directory = cx.update(|cx| registry_directory(cx));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "one.rs": "" }))
            .await;
        fs.insert_tree(path!("/other"), json!({ "file.rs": "" }))
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        cx.update(|cx| zed_mcp_server_for_project(&project, cx))
            .expect("a server command");

        let (worktree, _) = project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path!("/other"), true, cx)
            })
            .await
            .expect("adding a worktree");
        let roots = |directory: &Path| {
            read_registry(directory)
                .into_iter()
                .flat_map(|candidate| candidate.entry.roots)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            roots(&directory),
            [
                PathBuf::from(path!("/root")),
                PathBuf::from(path!("/other"))
            ]
        );

        let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
        project.update(cx, |project, cx| project.remove_worktree(worktree_id, cx));
        assert_eq!(roots(&directory), [PathBuf::from(path!("/root"))]);
    }
}
