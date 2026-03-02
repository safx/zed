use std::sync::Arc;

use gpui::*;
use settings::{KeymapFile, DEFAULT_KEYMAP_PATH};
use util::ResultExt as _;

fn main() {
    if std::env::args().any(|arg| arg == "--printenv") {
        util::shell_env::print_env();
        return;
    }

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

                    if let Some(bindings) =
                        KeymapFile::load_asset_allow_partial_failure(DEFAULT_KEYMAP_PATH, cx)
                            .log_err()
                    {
                        cx.bind_keys(bindings);
                    }

                    cx.open_window(
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
                            if let Ok(cwd) = std::env::current_dir() {
                                project
                                    .update(cx, |project, cx| {
                                        project.find_or_create_worktree(&cwd, true, cx)
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
