use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use futures::StreamExt as _;
use gpui::*;
use settings::{KeymapFile, DEFAULT_KEYMAP_PATH};
use ui::ActiveTheme;
use util::ResultExt as _;

#[derive(Parser)]
#[command(name = "agentium")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
}

#[derive(clap::Subcommand)]
enum WorkspaceAction {
    New { path: PathBuf },
}

fn agentium_socket_path() -> PathBuf {
    util::paths::home_dir()
        .join(".local")
        .join("share")
        .join("agentium")
        .join("agentium.sock")
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

fn start_workspace_ipc_listener(
    socket_path: PathBuf,
    path_sender: futures::channel::mpsc::UnboundedSender<PathBuf>,
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
            if let Ok(path_str) = std::str::from_utf8(&buffer[..n]) {
                let path = PathBuf::from(path_str);
                if path_sender.unbounded_send(path).is_err() {
                    break;
                }
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

    let args = Args::parse();

    let initial_workspace_path = match args.command {
        Some(Command::Workspace {
            action: WorkspaceAction::New { path },
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

    Application::new()
        .with_assets(assets::Assets)
        .run(|cx: &mut App| {
            release_channel::init(semver::Version::new(0, 1, 0), cx);
            settings::init(cx);
            theme::init(theme::LoadThemes::JustBase, cx);

            *theme::SystemAppearance::global_mut(cx) =
                theme::SystemAppearance(theme::Appearance::Dark);
            theme::GlobalTheme::reload_theme(cx);
            load_embedded_fonts(cx);
            let clock = Arc::new(clock::RealSystemClock);
            let http = Arc::new(http_client::HttpClientWithUrl::new(
                Arc::new(http_client::BlockedHttpClient::new()),
                "https://localhost",
                None,
            ));
            let client = client::Client::new(clock, http, cx);
            client::init(&client, cx);
            project::Project::init(&client, cx);

            let fs = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
            <dyn fs::Fs>::set_global(fs.clone(), cx);

            let languages = Arc::new(language::LanguageRegistry::new(
                cx.background_executor().clone(),
            ));
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
                let session = session::Session::new(uuid::Uuid::new_v4().to_string()).await;

                cx.update(|cx| {
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
                    git_ui::init(cx);
                    search::init(cx);
                    file_finder::init(cx);
                    markdown_preview::init(cx);

                    settings::SettingsStore::update_global(cx, |store, cx| {
                        _ = store.set_user_settings(
                            r#"{"active_pane_modifiers": {"inactive_opacity": 0.65}}"#,
                            cx,
                        );
                    });

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

                    if let Some(bindings) =
                        KeymapFile::load_asset_allow_partial_failure(DEFAULT_KEYMAP_PATH, cx)
                            .log_err()
                    {
                        cx.bind_keys(bindings);
                    }

                    let worktree_path = initial_workspace_path
                        .or_else(|| std::env::current_dir().ok());

                    let window_handle = cx
                        .open_window(
                            WindowOptions {
                                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                                    None,
                                    size(px(1200.0), px(800.0)),
                                    cx,
                                ))),
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

                    if let Some(window_handle) = window_handle {
                        let (path_sender, mut path_receiver) =
                            futures::channel::mpsc::unbounded::<PathBuf>();

                        start_workspace_ipc_listener(socket_path, path_sender).log_err();

                        cx.spawn({
                            async move |cx| {
                                while let Some(path) = path_receiver.next().await {
                                    window_handle
                                        .update(cx, |app, window, cx| {
                                            app.add_workspace_with_path(path, window, cx);
                                            window.activate_window();
                                            cx.activate(true);
                                        })
                                        .log_err();
                                }
                            }
                        })
                        .detach();
                    }

                    cx.activate(true);
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
