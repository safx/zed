use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{CommandFactory, Parser};
use futures::StreamExt as _;
use gpui::*;
use settings::{
    KeymapFile, SettingsStore, ThemeName, ThemeSelection, watch_config_file, DEFAULT_KEYMAP_PATH,
};
use ui::ActiveTheme;
use util::ResultExt as _;
use workspace::SplitDirection;

actions!(agentium, [Quit]);

fn quit(_: &Quit, cx: &mut App) {
    cx.quit();
}

#[derive(Parser)]
#[command(name = "agentium")]
struct Args {
    /// Theme name (e.g. "One Dark", "Ayu Dark", "Gruvbox Dark")
    #[arg(long)]
    theme: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    Arena {
        #[command(subcommand)]
        action: ArenaAction,
    },
    /// Pane operations
    Pane {
        #[command(subcommand)]
        action: PaneAction,
    },
    /// Tab operations
    Tab {
        #[command(subcommand)]
        action: TabAction,
    },
    /// Generate shell completions
    Completions {
        /// The shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Claude Code hook integration
    Claude {
        #[command(subcommand)]
        action: ClaudeAction,
    },
    /// Change the theme of the running instance
    Theme {
        /// Theme name (e.g. "One Dark", "Ayu Dark", "Gruvbox Dark")
        name: String,
    },
    /// Wait until the running Agentium instance is ready to accept IPC commands
    Ready {
        /// Timeout in seconds (default: 30)
        #[arg(long, short, default_value = "30")]
        timeout: u64,
    },
}

#[derive(clap::Subcommand)]
enum PaneAction {
    /// Split the active pane
    Split {
        /// Split horizontally (new pane to the right, or left with --before)
        #[arg(long, conflicts_with = "vertical")]
        horizontal: bool,
        /// Split vertically (new pane below, or above with --before). This is the default.
        #[arg(long, conflicts_with = "horizontal")]
        vertical: bool,
        /// Place the new pane before (left for horizontal, above for vertical)
        #[arg(long)]
        before: bool,
        #[arg(long, default_value = "terminal")]
        r#type: PaneContentType,
        #[arg(long)]
        keep_focus: bool,
        #[arg(last = true)]
        command: Vec<String>,
    },
}

#[derive(clap::Subcommand)]
enum TabAction {
    /// Add a new tab to the active pane
    New {
        #[arg(long, default_value = "terminal")]
        r#type: PaneContentType,
        #[arg(last = true)]
        command: Vec<String>,
    },
}

#[derive(clap::ValueEnum, Clone)]
enum PaneContentType {
    Terminal,
    Diff,
    BranchDiff,
    GitStatus,
    ProjectSearch,
    GitGraph,
}

#[derive(clap::Subcommand)]
enum ArenaAction {
    New { path: PathBuf },
}

#[derive(clap::Subcommand)]
enum ClaudeAction {
    Hook {
        #[command(subcommand)]
        event: ClaudeHookEvent,
    },
    /// Claude Code statusline pass-through (extracts rate limits)
    Statusline,
}

#[derive(clap::Subcommand)]
enum ClaudeHookEvent {
    SessionStart,
    Stop,
    Notification,
    UserPromptSubmit,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
}

fn percent_decode(input: &str) -> String {
    let mut output = Vec::with_capacity(input.len());
    let mut bytes = input.as_bytes().iter();
    while let Some(&byte) = bytes.next() {
        if byte == b'%' {
            let hi = bytes.next().copied().unwrap_or(0);
            let lo = bytes.next().copied().unwrap_or(0);
            if let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo)) {
                output.push(h << 4 | l);
                continue;
            }
            output.push(b'%');
            output.push(hi);
            output.push(lo);
        } else {
            output.push(byte);
        }
    }
    String::from_utf8(output).unwrap_or_else(|_| input.to_string())
}

/// Merges user settings JSON with agentium-specific defaults.
/// User file values take precedence; defaults fill in missing keys.
fn merge_settings_with_defaults(
    user_content: &str,
    defaults: &serde_json::Value,
) -> String {
    let mut merged = defaults.clone();
    if let Ok(user_value) = serde_json::from_str::<serde_json::Value>(user_content) {
        if let (Some(merged_obj), Some(user_obj)) = (merged.as_object_mut(), user_value.as_object())
        {
            for (key, value) in user_obj {
                merged_obj.insert(key.clone(), value.clone());
            }
        }
    }
    merged.to_string()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn truncate_string(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_index, _)) => s[..byte_index].to_string(),
        None => s.to_string(),
    }
}

fn agentium_socket_path() -> PathBuf {
    util::paths::home_dir()
        .join(".local")
        .join("share")
        .join("agentium")
        .join("agentium.sock")
}

enum IpcMessage {
    WorkspacePath(PathBuf),
    ClaudeSessionStart { session_id: String, ancestor_pids: Vec<u32> },
    ClaudeStop { session_id: String, ancestor_pids: Vec<u32>, title: String },
    ClaudeNotification { session_id: String, ancestor_pids: Vec<u32>, title: String },
    ClaudeUserPromptSubmit { session_id: String, ancestor_pids: Vec<u32>, prompt: String },
    ClaudePermissionRequest { session_id: String, ancestor_pids: Vec<u32> },
    ClaudePostToolUse { session_id: String, ancestor_pids: Vec<u32> },
    PaneSplit {
        direction: SplitDirection,
        content_type: agentium::PaneContentType,
        keep_focus: bool,
        command: Vec<String>,
    },
    TabNew {
        content_type: agentium::PaneContentType,
        command: Vec<String>,
    },
    ChangeTheme {
        name: String,
    },
    ClaudeStatusline {
        five_hour_used_pct: f32,
        five_hour_resets_at: i64,
        seven_day_used_pct: f32,
        seven_day_resets_at: i64,
    },
}

fn try_send_path_to_running_instance(
    socket_path: &PathBuf,
    workspace_path: &PathBuf,
) -> anyhow::Result<()> {
    let socket = UnixDatagram::unbound()?;
    socket.connect(socket_path)?;
    let path_bytes = workspace_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?;
    socket.send(path_bytes.as_bytes())?;
    Ok(())
}

fn get_ancestor_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let mut current = std::os::unix::process::parent_id();
    for _ in 0..10 {
        if current <= 1 {
            break;
        }
        pids.push(current);
        match std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &current.to_string()])
            .output()
        {
            Ok(output) => match String::from_utf8(output.stdout)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
            {
                Some(ppid) => current = ppid,
                None => break,
            },
            Err(_) => break,
        }
    }
    pids
}

fn start_ipc_listener(
    socket_path: PathBuf,
    msg_sender: futures::channel::mpsc::UnboundedSender<IpcMessage>,
) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if socket_path.exists() {
        let probe = UnixDatagram::unbound()?;
        match probe.connect(&socket_path) {
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "another instance is already listening on the socket"
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(&socket_path)?;
            }
            Err(err) => return Err(err.into()),
        }
    }

    let listener = UnixDatagram::bind(&socket_path)?;

    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok(n) = listener.recv(&mut buffer) {
            let bytes = &buffer[..n];
            let msg = if bytes.first() == Some(&b'{') {
                match serde_json::from_slice::<serde_json::Value>(bytes) {
                    Ok(json) => {
                        let session_id = json["session_id"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let ancestor_pids: Vec<u32> = json["ancestor_pids"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_u64().map(|n| n as u32))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let title = json["title"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let prompt = json["prompt"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        match json["type"].as_str() {
                            Some("claude_session_start") => {
                                IpcMessage::ClaudeSessionStart { session_id, ancestor_pids }
                            }
                            Some("claude_stop") => {
                                IpcMessage::ClaudeStop { session_id, ancestor_pids, title }
                            }
                            Some("claude_notification") => {
                                IpcMessage::ClaudeNotification { session_id, ancestor_pids, title }
                            }
                            Some("claude_user_prompt_submit") => {
                                IpcMessage::ClaudeUserPromptSubmit { session_id, ancestor_pids, prompt }
                            }
                            Some("claude_permission_request") => {
                                IpcMessage::ClaudePermissionRequest { session_id, ancestor_pids }
                            }
                            Some("claude_post_tool_use") => {
                                IpcMessage::ClaudePostToolUse { session_id, ancestor_pids }
                            }
                            Some("pane_split") => {
                                let direction = match json["direction"].as_str() {
                                    Some("right") => SplitDirection::Right,
                                    Some("left") => SplitDirection::Left,
                                    Some("down") => SplitDirection::Down,
                                    Some("up") => SplitDirection::Up,
                                    _ => continue,
                                };
                                let content_type = match json["content_type"].as_str() {
                                    Some("terminal") => agentium::PaneContentType::Terminal,
                                    Some("diff") => agentium::PaneContentType::Diff,
                                    Some("branch-diff") => agentium::PaneContentType::BranchDiff,
                                    Some("git-status") => agentium::PaneContentType::GitStatus,
                                    Some("project-search") => agentium::PaneContentType::ProjectSearch,
                                    Some("git-graph") => agentium::PaneContentType::GitGraph,
                                    _ => continue,
                                };
                                let keep_focus = json["keep_focus"].as_bool().unwrap_or(false);
                                let command: Vec<String> = json["command"]
                                    .as_array()
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(String::from))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                IpcMessage::PaneSplit { direction, content_type, keep_focus, command }
                            }
                            Some("claude_statusline") => {
                                let five_hour = &json["rate_limits"]["five_hour"];
                                let seven_day = &json["rate_limits"]["seven_day"];
                                IpcMessage::ClaudeStatusline {
                                    five_hour_used_pct: five_hour["used_percentage"]
                                        .as_f64()
                                        .unwrap_or(0.0) as f32,
                                    five_hour_resets_at: five_hour["resets_at"]
                                        .as_i64()
                                        .unwrap_or(0),
                                    seven_day_used_pct: seven_day["used_percentage"]
                                        .as_f64()
                                        .unwrap_or(0.0) as f32,
                                    seven_day_resets_at: seven_day["resets_at"]
                                        .as_i64()
                                        .unwrap_or(0),
                                }
                            }
                            Some("tab_new") => {
                                let content_type = match json["content_type"].as_str() {
                                    Some("terminal") => agentium::PaneContentType::Terminal,
                                    Some("diff") => agentium::PaneContentType::Diff,
                                    Some("branch-diff") => agentium::PaneContentType::BranchDiff,
                                    Some("git-status") => agentium::PaneContentType::GitStatus,
                                    Some("project-search") => agentium::PaneContentType::ProjectSearch,
                                    Some("git-graph") => agentium::PaneContentType::GitGraph,
                                    _ => continue,
                                };
                                let command: Vec<String> = json["command"]
                                    .as_array()
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(String::from))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                IpcMessage::TabNew { content_type, command }
                            }
                            Some("change_theme") => {
                                let name = json["name"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                if name.is_empty() {
                                    continue;
                                }
                                IpcMessage::ChangeTheme { name }
                            }
                            _ => continue,
                        }
                    }
                    Err(_) => continue,
                }
            } else if let Ok(path_str) = std::str::from_utf8(bytes) {
                IpcMessage::WorkspacePath(PathBuf::from(path_str))
            } else {
                continue;
            };
            if msg_sender.unbounded_send(msg).is_err() {
                break;
            }
        }
    });

    Ok(())
}

fn main() {
    if std::env::args().any(|arg| arg == "--printenv") {
        util::shell_env::print_env();
        return;
    }

    paths::set_app_name("Agentium");

    let args = Args::parse();
    let theme_name = args.theme;

    let initial_workspace_path = match args.command {
        Some(Command::Completions { shell }) => {
            clap_complete::generate(
                shell,
                &mut Args::command(),
                "agentium",
                &mut std::io::stdout(),
            );
            return;
        }
        Some(Command::Claude {
            action: ClaudeAction::Hook { event },
        }) => {
            let json: serde_json::Value =
                serde_json::from_reader(std::io::stdin()).unwrap_or_default();
            let session_id = json["session_id"].as_str().unwrap_or("");
            let ancestor_pids = get_ancestor_pids();

            let msg_type = match event {
                ClaudeHookEvent::SessionStart => "claude_session_start",
                ClaudeHookEvent::Stop => "claude_stop",
                ClaudeHookEvent::Notification => "claude_notification",
                ClaudeHookEvent::UserPromptSubmit => "claude_user_prompt_submit",
                ClaudeHookEvent::PermissionRequest => "claude_permission_request",
                ClaudeHookEvent::PostToolUse => "claude_post_tool_use",
                // Both map to the same IPC type: both transition WaitingPermission → Running.
                ClaudeHookEvent::PostToolUseFailure => "claude_post_tool_use",
            };
            let mut msg = serde_json::json!({
                "type": msg_type,
                "session_id": session_id,
                "ancestor_pids": ancestor_pids,
            });
            let title = json["title"].as_str()
                .or_else(|| json["message"].as_str());
            if let Some(title) = title {
                msg["title"] = serde_json::Value::String(truncate_string(title, 500));
            }
            if let Some(prompt) = json["prompt"].as_str() {
                msg["prompt"] = serde_json::Value::String(truncate_string(prompt, 500));
            }

            let socket_path = agentium_socket_path();
            if let Ok(socket) = UnixDatagram::unbound() {
                if socket.connect(&socket_path).is_ok() {
                    socket.send(msg.to_string().as_bytes()).ok();
                }
            }
            return;
        }
        Some(Command::Claude {
            action: ClaudeAction::Statusline,
        }) => {
            use std::io::{Read, Write};

            let mut input = String::new();
            if let Err(err) = std::io::stdin().read_to_string(&mut input) {
                eprintln!("agentium statusline: failed to read stdin: {err}");
                return;
            }

            if let Err(error) = std::io::stdout()
                .write_all(input.as_bytes())
                .and_then(|_| std::io::stdout().flush())
            {
                eprintln!("agentium statusline: failed to write stdout: {error}");
            }

            let json: serde_json::Value = match serde_json::from_str(&input) {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("agentium statusline: failed to parse JSON: {err}");
                    return;
                }
            };

            if let Some(rate_limits) = json.get("rate_limits") {
                let msg = serde_json::json!({
                    "type": "claude_statusline",
                    "rate_limits": rate_limits,
                });
                let socket_path = agentium_socket_path();
                match UnixDatagram::unbound().and_then(|socket| {
                    socket.connect(&socket_path)?;
                    socket.send(msg.to_string().as_bytes())?;
                    Ok(())
                }) {
                    Ok(()) => {}
                    Err(err) => {
                        eprintln!("agentium statusline: failed to send IPC: {err}");
                    }
                }
            }
            return;
        }
        Some(Command::Pane { action }) => {
            let (direction, content_type, keep_focus, command) = match action {
                PaneAction::Split {
                    horizontal,
                    before,
                    r#type,
                    keep_focus,
                    command,
                    ..
                } => {
                    let direction = match (horizontal, before) {
                        (true, true) => "left",
                        (true, false) => "right",
                        (false, true) => "up",
                        (false, false) => "down",
                    };
                    (direction, r#type, keep_focus, command)
                }
            };
            let content_type_str = match content_type {
                PaneContentType::Terminal => "terminal",
                PaneContentType::Diff => "diff",
                PaneContentType::BranchDiff => "branch-diff",
                PaneContentType::GitStatus => "git-status",
                PaneContentType::ProjectSearch => "project-search",
                PaneContentType::GitGraph => "git-graph",
            };
            let msg = serde_json::json!({
                "type": "pane_split",
                "direction": direction,
                "content_type": content_type_str,
                "keep_focus": keep_focus,
                "command": command,
            });
            let socket_path = agentium_socket_path();
            if let Ok(socket) = UnixDatagram::unbound() {
                if socket.connect(&socket_path).is_ok() {
                    socket.send(msg.to_string().as_bytes()).ok();
                }
            }
            return;
        }
        Some(Command::Tab { action }) => {
            let (content_type, command) = match action {
                TabAction::New { r#type, command } => (r#type, command),
            };
            let content_type_str = match content_type {
                PaneContentType::Terminal => "terminal",
                PaneContentType::Diff => "diff",
                PaneContentType::BranchDiff => "branch-diff",
                PaneContentType::GitStatus => "git-status",
                PaneContentType::ProjectSearch => "project-search",
                PaneContentType::GitGraph => "git-graph",
            };
            let msg = serde_json::json!({
                "type": "tab_new",
                "content_type": content_type_str,
                "command": command,
            });
            let socket_path = agentium_socket_path();
            if let Ok(socket) = UnixDatagram::unbound() {
                if socket.connect(&socket_path).is_ok() {
                    socket.send(msg.to_string().as_bytes()).ok();
                }
            }
            return;
        }
        Some(Command::Ready { timeout }) => {
            let socket_path = agentium_socket_path();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                if let Ok(socket) = UnixDatagram::unbound() {
                    if socket.connect(&socket_path).is_ok() {
                        return;
                    }
                }
                if std::time::Instant::now() >= deadline {
                    eprintln!("error: timed out waiting for Agentium ({}s)", timeout);
                    std::process::exit(1);
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
        Some(Command::Theme { name }) => {
            let msg = serde_json::json!({
                "type": "change_theme",
                "name": name,
            });
            let socket_path = agentium_socket_path();
            if let Ok(socket) = UnixDatagram::unbound() {
                if socket.connect(&socket_path).is_ok() {
                    let _ = socket.send(msg.to_string().as_bytes());
                }
            }
            return;
        }
        Some(Command::Arena {
            action: ArenaAction::New { path },
        }) => match std::fs::canonicalize(&path) {
            Ok(canonical) => Some(canonical),
            Err(err) => {
                eprintln!("error: cannot resolve path '{}': {}", path.display(), err);
                std::process::exit(1);
            }
        },
        None => None,
    };

    if let Some(ref workspace_path) = initial_workspace_path {
        let socket_path = agentium_socket_path();
        if try_send_path_to_running_instance(&socket_path, workspace_path).is_ok() {
            return;
        }
    }

    let socket_path = agentium_socket_path();

    let app = Application::with_platform(gpui_platform::current_platform(false))
        .with_assets(assets::Assets);

    app.on_open_urls({
        let socket_path = socket_path.clone();
        move |urls| {
            for url in urls {
                let path = if let Some(path) = url.strip_prefix("file://") {
                    PathBuf::from(percent_decode(path))
                } else {
                    continue;
                };
                if path.is_dir() {
                    try_send_path_to_running_instance(&socket_path, &path).log_err();
                }
            }
        }
    });

    app.run(move |cx: &mut App| {
            release_channel::init(semver::Version::new(0, 1, 0), cx);
            settings::init(cx);

            #[cfg(unix)]
            cx.background_executor()
                .spawn(async {
                    util::load_login_shell_environment().await.log_err();
                })
                .detach();

            theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
            *theme::SystemAppearance::global_mut(cx) =
                theme::SystemAppearance(theme::Appearance::Dark);
            theme_settings::reload_theme(cx);

            if let Some(ref name) = theme_name {
                let registry = theme::ThemeRegistry::default_global(cx);
                if registry.get(name).is_err() {
                    let available = registry.list_names();
                    eprintln!("error: unknown theme '{name}'");
                    eprintln!("Available themes:");
                    for theme in &available {
                        eprintln!("  - {theme}");
                    }
                    std::process::exit(1);
                }
            }
            load_embedded_fonts(cx);

            cx.on_action(quit);
            cx.set_menus(vec![Menu {
                name: "Agentium".into(),
                items: vec![
                    MenuItem::action("Quit Agentium", Quit),
                ],
                disabled: false,
            }]);

            let clock = Arc::new(clock::RealSystemClock);
            let http = Arc::new(http_client::HttpClientWithUrl::new(
                Arc::new(reqwest_client::ReqwestClient::new()),
                "https://localhost",
                None,
            ));
            cx.set_http_client(Arc::new(reqwest_client::ReqwestClient::new()));
            let client = client::Client::new(clock, http, cx);
            client::init(&client, cx);
            project::Project::init(&client, cx);

            let fs = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
            <dyn fs::Fs>::set_global(fs.clone(), cx);

            let mut languages = language::LanguageRegistry::new(
                cx.background_executor().clone(),
            );
            languages.set_language_server_download_dir(paths::languages_dir().clone());
            let languages = Arc::new(languages);
            let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
            let node_runtime = node_runtime::NodeRuntime::unavailable();

            language::disable_wasm_parsers();
            languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);

            let client_for_window = client.clone();
            let user_store_for_window = user_store.clone();
            let languages_for_window = languages.clone();
            let fs_for_window = fs.clone();
            let node_runtime_for_window = node_runtime.clone();

            cx.spawn(async move |cx| {
                let app_db = db::AppDatabase::new();
                let session = session::Session::new(
                    uuid::Uuid::new_v4().to_string(),
                    db::kvp::KeyValueStore::from_app_db(&app_db),
                ).await;

                cx.update(|cx| {
                    cx.set_global(app_db);
                    let app_session = cx.new(|cx| session::AppSession::new(session, cx));
                    let workspace_store =
                        cx.new(|cx| workspace::WorkspaceStore::new(client.clone(), cx));

                    let app_state = Arc::new(workspace::AppState {
                        languages: languages.clone(),
                        client: client.clone(),
                        user_store: user_store.clone(),
                        workspace_store,
                        fs: fs.clone(),
                        build_window_options: |_, _| Default::default(),
                        node_runtime: node_runtime.clone(),
                        session: app_session,
                    });

                    workspace::init(app_state.clone(), cx);
                    editor::init(cx);
                    image_viewer::init(cx);
                    git_ui::init(cx);
                    search::init(cx);
                    file_finder::init(cx);
                    markdown_preview::init(cx);

                    let (mut settings_file_rx, _settings_watcher) = watch_config_file(
                        cx.background_executor(),
                        fs.clone(),
                        paths::settings_file().clone(),
                    );

                    let agentium_defaults = {
                        let mut defaults = serde_json::json!({
                            "active_pane_modifiers": {"inactive_opacity": 0.65},
                        });
                        if let Some(ref name) = theme_name {
                            defaults["theme"] = serde_json::json!(name);
                        }
                        defaults
                    };

                    // Initial load: merge user settings file with agentium defaults.
                    // User file values take precedence; agentium defaults fill in gaps.
                    let initial_content = cx
                        .foreground_executor()
                        .block_on(settings_file_rx.next())
                        .unwrap_or_default();
                    let merged =
                        merge_settings_with_defaults(&initial_content, &agentium_defaults);
                    SettingsStore::update_global(cx, |store, cx| {
                        _ = store.set_user_settings(&merged, cx);
                    });

                    // Watch for changes and re-merge.
                    cx.spawn({
                        let agentium_defaults = agentium_defaults.clone();
                        async move |cx| {
                            let _settings_watcher = _settings_watcher;
                            while let Some(content) = settings_file_rx.next().await {
                                let merged =
                                    merge_settings_with_defaults(&content, &agentium_defaults);
                                cx.update_global(|store: &mut SettingsStore, cx| {
                                    _ = store.set_user_settings(&merged, cx);
                                });
                            }
                        }
                    })
                    .detach();

                    cx.set_global(workspace::PaneSearchBarCallbacks {
                        setup_search_bar: |languages, toolbar, window, cx| {
                            let search_bar =
                                cx.new(|cx| search::BufferSearchBar::new(languages, window, cx));
                            toolbar.update(cx, |toolbar, cx| {
                                toolbar.add_item(search_bar, window, cx);
                            });
                        },
                        wrap_div_with_search_actions:
                            search::buffer_search::register_pane_search_actions,
                    });

                    app_state.languages.set_theme(cx.theme().clone());
                    cx.observe_global::<theme::GlobalTheme>({
                        let languages = app_state.languages.clone();
                        move |cx| {
                            languages.set_theme(cx.theme().clone());
                        }
                    })
                    .detach();

                    if let Some(bindings) =
                        KeymapFile::load_asset_allow_partial_failure(DEFAULT_KEYMAP_PATH, cx)
                            .log_err()
                    {
                        cx.bind_keys(bindings);
                    }

                    cx.bind_keys([
                        KeyBinding::new("cmd-1", agentium::ActivateArena { index: 0 }, Some("Workspace")),
                        KeyBinding::new("cmd-2", agentium::ActivateArena { index: 1 }, Some("Workspace")),
                        KeyBinding::new("cmd-3", agentium::ActivateArena { index: 2 }, Some("Workspace")),
                        KeyBinding::new("cmd-4", agentium::ActivateArena { index: 3 }, Some("Workspace")),
                        KeyBinding::new("cmd-5", agentium::ActivateArena { index: 4 }, Some("Workspace")),
                        KeyBinding::new("cmd-6", agentium::ActivateArena { index: 5 }, Some("Workspace")),
                        KeyBinding::new("cmd-7", agentium::ActivateArena { index: 6 }, Some("Workspace")),
                        KeyBinding::new("cmd-8", agentium::ActivateArena { index: 7 }, Some("Workspace")),
                        KeyBinding::new("cmd-9", agentium::ActivateArena { index: 8 }, Some("Workspace")),
                        KeyBinding::new("cmd-1", agentium::ActivateArena { index: 0 }, Some("Agentium")),
                        KeyBinding::new("cmd-2", agentium::ActivateArena { index: 1 }, Some("Agentium")),
                        KeyBinding::new("cmd-3", agentium::ActivateArena { index: 2 }, Some("Agentium")),
                        KeyBinding::new("cmd-4", agentium::ActivateArena { index: 3 }, Some("Agentium")),
                        KeyBinding::new("cmd-5", agentium::ActivateArena { index: 4 }, Some("Agentium")),
                        KeyBinding::new("cmd-6", agentium::ActivateArena { index: 5 }, Some("Agentium")),
                        KeyBinding::new("cmd-7", agentium::ActivateArena { index: 6 }, Some("Agentium")),
                        KeyBinding::new("cmd-8", agentium::ActivateArena { index: 7 }, Some("Agentium")),
                        KeyBinding::new("cmd-9", agentium::ActivateArena { index: 8 }, Some("Agentium")),
                        KeyBinding::new("ctrl-[", workspace::ActivatePreviousPane, Some("Workspace")),
                        KeyBinding::new("ctrl-]", workspace::ActivateNextPane, Some("Workspace")),
                        KeyBinding::new("cmd-[", workspace::ActivatePreviousItem::default(), Some("Pane")),
                        KeyBinding::new("cmd-]", workspace::ActivateNextItem::default(), Some("Pane")),
                    ]);

                    let worktree_path = initial_workspace_path;

                    let window_handle = cx
                        .open_window(
                            WindowOptions {
                                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                                    None,
                                    size(px(1200.0), px(800.0)),
                                    cx,
                                ))),
                                titlebar: Some(TitlebarOptions {
                                    title: None,
                                    appears_transparent: true,
                                    traffic_light_position: Some(point(px(9.0), px(9.0))),
                                }),
                                is_movable: false,
                                ..Default::default()
                            },
                            |window, cx| {
                                let project = project::Project::local(
                                    client_for_window,
                                    node_runtime_for_window,
                                    user_store_for_window,
                                    languages_for_window,
                                    fs_for_window,
                                    None,
                                    project::LocalProjectFlags::default(),
                                    cx,
                                );
                                if let Some(ref path) = worktree_path {
                                    project
                                        .update(cx, |project, cx| {
                                            project.find_or_create_worktree(path, true, cx)
                                        })
                                        .detach_and_log_err(cx);
                                }
                                let workspace_entity = cx.new(|cx| {
                                    workspace::Workspace::new(
                                        None,
                                        project.clone(),
                                        app_state.clone(),
                                        window,
                                        cx,
                                    )
                                });
                                window.set_window_title("Agentium");
                                cx.new(|cx| {
                                    agentium::AgentiumApp::new(
                                        workspace_entity,
                                        project,
                                        app_state,
                                        window,
                                        cx,
                                    )
                                })
                            },
                        )
                        .log_err();

                    cx.activate(true);

                    if let Some(window_handle) = window_handle {
                        let (msg_sender, mut msg_receiver) =
                            futures::channel::mpsc::unbounded::<IpcMessage>();

                        start_ipc_listener(socket_path, msg_sender).log_err();

                        cx.spawn({
                            async move |cx| {
                                while let Some(msg) = msg_receiver.next().await {
                                    match msg {
                                        IpcMessage::WorkspacePath(path) => {
                                            window_handle
                                                .update(cx, |app, window, cx| {
                                                    app.add_arena_with_path(
                                                        path, window, cx,
                                                    );
                                                    window.activate_window();
                                                    cx.activate(true);
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::ClaudeSessionStart {
                                            session_id,
                                            ancestor_pids,
                                        } => {
                                            window_handle
                                                .update(cx, |app, _window, cx| {
                                                    app.register_claude_session(
                                                        session_id, ancestor_pids, cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::ClaudeStop {
                                            session_id,
                                            ancestor_pids,
                                            title,
                                        } => {
                                            let title = if title.is_empty() { None } else { Some(title) };
                                            window_handle
                                                .update(cx, |app, _window, cx| {
                                                    app.mark_claude_session_ready(
                                                        &session_id, ancestor_pids, title, cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::ClaudeNotification {
                                            session_id,
                                            ancestor_pids,
                                            title,
                                        } => {
                                            window_handle
                                                .update(cx, |app, _window, cx| {
                                                    app.handle_claude_notification(
                                                        &session_id, ancestor_pids, title, cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::ClaudeUserPromptSubmit {
                                            session_id,
                                            ancestor_pids,
                                            prompt,
                                        } => {
                                            window_handle
                                                .update(cx, |app, _window, cx| {
                                                    app.set_claude_session_prompt(
                                                        &session_id, ancestor_pids, prompt, cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::ClaudePermissionRequest {
                                            session_id,
                                            ancestor_pids,
                                        } => {
                                            window_handle
                                                .update(cx, |app, _window, cx| {
                                                    app.handle_claude_permission_request(
                                                        &session_id, ancestor_pids, cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::ClaudePostToolUse {
                                            session_id,
                                            ancestor_pids,
                                        } => {
                                            window_handle
                                                .update(cx, |app, _window, cx| {
                                                    app.handle_claude_post_tool_use(
                                                        &session_id, ancestor_pids, cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::PaneSplit {
                                            direction,
                                            content_type,
                                            keep_focus,
                                            command,
                                        } => {
                                            window_handle
                                                .update(cx, |app, window, cx| {
                                                    app.handle_pane_split(
                                                        direction,
                                                        content_type,
                                                        keep_focus,
                                                        command,
                                                        window,
                                                        cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::ClaudeStatusline {
                                            five_hour_used_pct,
                                            five_hour_resets_at,
                                            seven_day_used_pct,
                                            seven_day_resets_at,
                                        } => {
                                            window_handle
                                                .update(cx, |app, _window, cx| {
                                                    app.update_rate_limits(
                                                        five_hour_used_pct,
                                                        five_hour_resets_at,
                                                        seven_day_used_pct,
                                                        seven_day_resets_at,
                                                        cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::TabNew {
                                            content_type,
                                            command,
                                        } => {
                                            window_handle
                                                .update(cx, |app, window, cx| {
                                                    app.handle_tab_new(
                                                        content_type,
                                                        command,
                                                        window,
                                                        cx,
                                                    );
                                                })
                                                .log_err();
                                        }
                                        IpcMessage::ChangeTheme { name } => {
                                            window_handle
                                                .update(cx, |_app, _window, cx| {
                                                    settings::SettingsStore::update_global(
                                                        cx,
                                                        |store, cx| {
                                                            let mut settings = store
                                                                .raw_user_settings()
                                                                .cloned()
                                                                .unwrap_or_default();
                                                            settings.content.theme.theme =
                                                                Some(ThemeSelection::Static(
                                                                    ThemeName(name.into()),
                                                                ));
                                                            if let Ok(json) =
                                                                serde_json::to_string(&settings)
                                                            {
                                                                _ = store.set_user_settings(&json, cx);
                                                            }
                                                        },
                                                    );
                                                })
                                                .log_err();
                                        }
                                    }
                                }
                            }
                        })
                        .detach();
                    }
                });
            })
            .detach();
        });
}

fn load_embedded_fonts(cx: &App) {
    let asset_source = cx.asset_source();
    let font_paths = asset_source.list("fonts").unwrap();
    let embedded_fonts = parking_lot::Mutex::new(Vec::new());
    let executor = cx.background_executor();

    cx.foreground_executor().block_on(executor.scoped(|scope| {
        for font_path in &font_paths {
            if !font_path.ends_with(".ttf") {
                continue;
            }

            scope.spawn(async {
                let font_bytes = asset_source.load(font_path).unwrap().unwrap();
                embedded_fonts.lock().push(font_bytes);
            });
        }
    }));

    cx.text_system()
        .add_fonts(embedded_fonts.into_inner())
        .unwrap();
}
