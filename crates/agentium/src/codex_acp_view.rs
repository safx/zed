use std::{path::PathBuf, rc::Rc, sync::Arc};

use acp_thread::{AcpThread, AcpThreadEvent, AgentConnection, ThreadStatus};
use agent_client_protocol::schema::{v1 as acp, v2 as acp_v2};
use agent_servers::{AcpConnection, AcpLaunchOptions, AgentServer, AgentServerDelegate, CODEX_ID};
use agent_ui::{
    Agent, AgentConnectionStore, AgentThreadSource, ConversationView, RootThreadUpdated,
};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use fs::Fs;
use futures::future::Shared;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Global,
    IntoElement, Render, SharedString, Subscription, Task, WeakEntity, Window, prelude::*,
};
use parking_lot::Mutex;
use project::{
    Project,
    agent_server_store::{AgentServerCommand, AllAgentServersSettings, CustomAgentServerSettings},
};
use settings::{AgentConfigOptionValue, SettingsStore, update_settings_file};
use task::Shell;
use ui::{Indicator, prelude::*};
use workspace::{Item, Workspace, item::ItemEvent};

#[derive(Clone)]
pub struct LoginEnvironmentReady(pub Shared<futures::channel::oneshot::Receiver<()>>);
impl Global for LoginEnvironmentReady {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CodexAcpState {
    #[default]
    Idle,
    Running,
    Permission,
    Completed,
    Error,
}

impl CodexAcpState {
    pub(crate) fn indicator(self) -> Option<Indicator> {
        let color = match self {
            Self::Idle => return None,
            Self::Running => Color::Success,
            Self::Permission => Color::Warning,
            Self::Completed => Color::Accent,
            Self::Error => Color::Error,
        };
        Some(Indicator::dot().color(color))
    }
}

pub(crate) struct CodexAcpView {
    conversation: Entity<ConversationView>,
    focus_handle: FocusHandle,
    _connection_store: Entity<AgentConnectionStore>,
    working_directory: Option<PathBuf>,
    warning: Arc<Mutex<Option<String>>>,
    state: CodexAcpState,
    generation: u64,
    thread_id: Option<EntityId>,
    thread_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl CodexAcpView {
    pub(crate) fn new(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        working_directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let warning = Arc::new(Mutex::new(None));
        let server = Rc::new(CodexAcpServer {
            working_directory: working_directory.clone(),
            warning: warning.clone(),
        });
        Self::with_server(
            server,
            project,
            workspace,
            working_directory,
            warning,
            window,
            cx,
        )
    }

    fn with_server(
        server: Rc<dyn AgentServer>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        working_directory: Option<PathBuf>,
        warning: Arc<Mutex<Option<String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connection_store = cx.new(|cx| AgentConnectionStore::new_isolated(project.clone(), cx));
        let work_dirs = working_directory
            .as_ref()
            .map(|directory| workspace::PathList::new(&[directory]));
        let conversation = cx.new(|cx| {
            ConversationView::new(
                server,
                connection_store.clone(),
                Agent::Custom {
                    id: CODEX_ID.into(),
                },
                None,
                None,
                work_dirs,
                None,
                None,
                workspace,
                project,
                None,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe(&conversation, |this, _, _: &RootThreadUpdated, cx| {
                this.bind_thread(cx);
            }),
            cx.observe(&conversation, |this, conversation, cx| {
                this.focus_handle = conversation.read(cx).focus_handle(cx);
                if conversation.read(cx).load_error().is_some() {
                    this.state = CodexAcpState::Error;
                }
                cx.emit(ItemEvent::UpdateTab);
                cx.notify();
            }),
        ];
        let focus_handle = conversation.read(cx).focus_handle(cx);
        Self {
            conversation,
            focus_handle,
            _connection_store: connection_store,
            working_directory,
            warning,
            state: CodexAcpState::Idle,
            generation: 0,
            thread_id: None,
            thread_subscription: None,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn conversation_id(&self) -> EntityId {
        self.conversation.entity_id()
    }

    pub(crate) fn state(&self) -> CodexAcpState {
        self.state
    }

    fn bind_thread(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.conversation.read(cx).root_thread(cx) else {
            return;
        };
        if self.thread_id == Some(thread.entity_id()) {
            return;
        }
        self.thread_id = Some(thread.entity_id());
        self.state = CodexAcpState::Idle;
        self.generation = thread.read(cx).activity_generation();
        self.thread_subscription = Some(cx.subscribe(&thread, |this, thread, event, cx| {
            this.handle_thread_event(thread, event, cx);
        }));
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn handle_thread_event(
        &mut self,
        thread: Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let thread = thread.read(cx);
        let generation = thread.activity_generation();
        match event {
            AcpThreadEvent::Error | AcpThreadEvent::LoadError(_) | AcpThreadEvent::Refusal => {
                self.state = CodexAcpState::Error
            }
            AcpThreadEvent::Stopped {
                activity_generation,
                stop_reason,
                ..
            } if *activity_generation == generation => {
                self.state = if thread.had_error()
                    || matches!(stop_reason, Some(acp_v2::StopReason::Refusal))
                {
                    CodexAcpState::Error
                } else if matches!(stop_reason, Some(acp_v2::StopReason::Cancelled)) {
                    CodexAcpState::Idle
                } else if self.generation == generation && self.state != CodexAcpState::Idle {
                    CodexAcpState::Completed
                } else {
                    self.state
                };
            }
            AcpThreadEvent::StatusChanged
            | AcpThreadEvent::ToolAuthorizationRequested(_)
            | AcpThreadEvent::ToolAuthorizationReceived(_)
            | AcpThreadEvent::ElicitationRequested(_)
            | AcpThreadEvent::ElicitationResponded(_) => {
                if thread.is_waiting_for_confirmation() {
                    self.state = CodexAcpState::Permission;
                } else if thread.status() == ThreadStatus::Generating {
                    self.state = CodexAcpState::Running;
                    self.generation = generation;
                }
            }
            _ => {}
        }
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    pub(crate) fn acknowledge(&mut self, cx: &mut Context<Self>) {
        if self.state == CodexAcpState::Completed {
            self.state = CodexAcpState::Idle;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }
    }
}

impl Item for CodexAcpView {
    type Event = ItemEvent;
    fn include_in_nav_history() -> bool {
        false
    }
    fn to_item_events(event: &ItemEvent, emit: &mut dyn FnMut(ItemEvent)) {
        emit(*event);
    }
    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.conversation
            .read(cx)
            .root_thread(cx)
            .and_then(|thread| thread.read(cx).title())
            .unwrap_or_else(|| "Codex (ACP)".into())
    }
    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(
            format!(
                "Codex (ACP) — {}",
                self.working_directory
                    .as_ref()
                    .map(|directory| directory.display().to_string())
                    .unwrap_or_default()
            )
            .into(),
        )
    }
}
impl EventEmitter<ItemEvent> for CodexAcpView {}
impl Focusable for CodexAcpView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl Render for CodexAcpView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.is_window_active() && self.focus_handle(cx).contains_focused(window, cx) {
            self.acknowledge(cx);
            self.conversation.update(cx, |conversation, cx| {
                conversation.dismiss_notifications(cx);
            });
        }
        let warning = self.warning.lock().clone();
        v_flex()
            .size_full()
            .overflow_hidden()
            .when_some(warning, |element, warning| {
                element.child(
                    div()
                        .p_2()
                        .text_sm()
                        .text_color(cx.theme().status().warning)
                        .child(warning),
                )
            })
            .child(self.conversation.clone())
    }
}

struct CodexAcpServer {
    working_directory: Option<PathBuf>,
    warning: Arc<Mutex<Option<String>>>,
}

fn fallback_command() -> AgentServerCommand {
    AgentServerCommand {
        path: "codex-acp".into(),
        args: Vec::new(),
        env: None,
    }
}

struct CodexAcpLaunch {
    command: AgentServerCommand,
    options: AcpLaunchOptions,
    default_mode: Option<acp::SessionModeId>,
    default_config_options: HashMap<String, AgentConfigOptionValue>,
    warning: Option<String>,
}

fn build_launch(
    directory: PathBuf,
    directory_environment: Option<HashMap<String, String>>,
    proxy_environment: HashMap<String, String>,
    settings: Option<&CustomAgentServerSettings>,
) -> Result<CodexAcpLaunch> {
    let (mut command, default_mode, default_config_options) = match settings {
        None => (fallback_command(), None, HashMap::default()),
        Some(CustomAgentServerSettings::Custom {
            command,
            default_mode,
            default_config_options,
            ..
        }) => (
            command.clone(),
            default_mode.clone().map(acp::SessionModeId::new),
            default_config_options.clone(),
        ),
        Some(CustomAgentServerSettings::Registry { .. }) => anyhow::bail!(
            "Codex (ACP) is experimental and requires a custom agent_servers.codex-acp command. ACP Registry installation is not supported yet."
        ),
    };
    let warning = directory_environment.is_none().then(|| {
        "Could not load the directory environment. Using the inherited environment.".to_owned()
    });
    let explicit_environment = command.env.take().unwrap_or_default();
    let mut environment = directory_environment.unwrap_or_default();
    environment.extend(proxy_environment);
    environment.extend(explicit_environment);
    command.env = Some(environment);
    Ok(CodexAcpLaunch {
        command,
        options: AcpLaunchOptions {
            working_directory: Some(directory),
            client_name: "agentium",
            terminate_on_drop_and_quit: true,
        },
        default_mode,
        default_config_options,
        warning,
    })
}

fn default_custom_settings() -> settings::CustomAgentServerSettings {
    settings::CustomAgentServerSettings::Custom {
        path: "codex-acp".into(),
        args: Vec::new(),
        env: HashMap::default(),
        default_mode: None,
        default_config_options: HashMap::default(),
        favorite_config_option_values: HashMap::default(),
    }
}

impl AgentServer for CodexAcpServer {
    fn logo(&self) -> IconName {
        IconName::AiOpenAi
    }
    fn agent_id(&self) -> project::AgentId {
        CODEX_ID.into()
    }
    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
    fn default_mode(&self, cx: &App) -> Option<acp::SessionModeId> {
        agent_servers::CustomAgentServer::new(CODEX_ID.into()).default_mode(cx)
    }
    fn default_config_option(&self, config_id: &str, cx: &App) -> Option<AgentConfigOptionValue> {
        agent_servers::CustomAgentServer::new(CODEX_ID.into()).default_config_option(config_id, cx)
    }
    fn set_default_mode(&self, mode: Option<acp::SessionModeId>, fs: Arc<dyn Fs>, cx: &mut App) {
        update_settings_file(fs, cx, move |settings, _| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(CODEX_ID.into())
                .or_insert_with(default_custom_settings);
            if let settings::CustomAgentServerSettings::Custom { default_mode, .. } = settings {
                *default_mode = mode.as_ref().map(ToString::to_string);
            }
        });
    }
    fn set_default_config_option(
        &self,
        config_id: &str,
        value: Option<AgentConfigOptionValue>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        let config_id = config_id.to_owned();
        update_settings_file(fs, cx, move |settings, _| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(CODEX_ID.into())
                .or_insert_with(default_custom_settings);
            if let settings::CustomAgentServerSettings::Custom {
                default_config_options,
                ..
            } = settings
            {
                if let Some(value) = &value {
                    default_config_options.insert(config_id.clone(), value.clone());
                } else {
                    default_config_options.remove(&config_id);
                }
            }
        });
    }
    fn favorite_config_option_value_ids(
        &self,
        config_id: &acp_v2::SessionConfigId,
        cx: &mut App,
    ) -> HashSet<acp_v2::SessionConfigValueId> {
        agent_servers::CustomAgentServer::new(CODEX_ID.into())
            .favorite_config_option_value_ids(config_id, cx)
    }
    fn toggle_favorite_config_option_value(
        &self,
        config_id: acp_v2::SessionConfigId,
        value_id: acp_v2::SessionConfigValueId,
        favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        update_settings_file(fs, cx, move |settings, _| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(CODEX_ID.into())
                .or_insert_with(default_custom_settings);
            if let settings::CustomAgentServerSettings::Custom {
                favorite_config_option_values,
                ..
            } = settings
            {
                let values = favorite_config_option_values
                    .entry(config_id.to_string())
                    .or_default();
                values.retain(|value| value != value_id.0.as_ref());
                if favorite {
                    values.push(value_id.to_string());
                }
            }
        });
    }
    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let directory = self.working_directory.clone();
        let warning = self.warning.clone();
        let environment_ready = cx.try_global::<LoginEnvironmentReady>().cloned();
        let store = project.read(cx).agent_server_store().downgrade();
        let environment = project.read(cx).environment().clone();
        cx.spawn(async move |cx| {
            let directory = directory.context("The Arena has no working directory")?;
            if let Some(ready) = environment_ready {
                ready
                    .0
                    .await
                    .context("Login shell environment loading was cancelled")?;
            }
            let directory_exists = project
                .read_with(cx, |project, _| project.fs().clone())
                .metadata(&directory)
                .await?
                .is_some_and(|metadata| metadata.is_dir);
            anyhow::ensure!(
                directory_exists,
                "Arena directory does not exist: {}",
                directory.display()
            );
            let directory_environment = environment
                .update(cx, |environment, cx| {
                    environment.local_directory_environment(
                        &Shell::System,
                        Arc::from(directory.as_path()),
                        cx,
                    )
                })
                .await;
            let proxy_environment = cx.update(agent_servers::load_proxy_env);
            let launch = cx.update(|cx| {
                cx.read_global(|store: &SettingsStore, _| {
                    build_launch(
                        directory,
                        directory_environment,
                        proxy_environment,
                        store.get::<AllAgentServersSettings>(None).get(CODEX_ID),
                    )
                })
            })?;
            *warning.lock() = launch.warning;
            let connection = AcpConnection::stdio_with_options(
                CODEX_ID.into(),
                project,
                launch.command,
                store,
                launch.default_mode,
                launch.default_config_options,
                launch.options,
                cx,
            )
            .await;
            match connection {
                Ok(connection) => Ok(Rc::new(connection) as Rc<dyn AgentConnection>),
                Err(error) => Err(error),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_servers::FakeAcpAgentServer;
    use fs::FakeFs;
    use gpui::{Action as _, TestAppContext, VisualTestContext};
    use workspace::{MultiWorkspace, Pane};

    #[test]
    fn codex_acp_launch_uses_each_arena_directory() {
        for directory in [PathBuf::from("/first"), PathBuf::from("/second")] {
            let launch = build_launch(
                directory.clone(),
                Some(HashMap::default()),
                HashMap::default(),
                None,
            )
            .expect("build Arena launch");
            assert_eq!(launch.options.working_directory, Some(directory));
            assert_eq!(launch.options.client_name, "agentium");
            assert!(launch.options.terminate_on_drop_and_quit);
            assert!(launch.warning.is_none());
        }
    }

    #[test]
    fn codex_acp_launch_prioritizes_explicit_environment() {
        let settings = CustomAgentServerSettings::Custom {
            command: AgentServerCommand {
                path: "configured-codex-acp".into(),
                args: Vec::new(),
                env: Some(HashMap::from_iter([
                    ("COLLISION".into(), "explicit".into()),
                    ("EXPLICIT_ONLY".into(), "explicit".into()),
                ])),
            },
            default_mode: None,
            default_config_options: HashMap::default(),
            favorite_config_option_values: HashMap::default(),
        };
        let launch = build_launch(
            "/arena".into(),
            Some(HashMap::from_iter([
                ("COLLISION".into(), "directory".into()),
                ("PROXY_COLLISION".into(), "directory".into()),
                ("DIRECTORY_ONLY".into(), "directory".into()),
            ])),
            HashMap::from_iter([
                ("COLLISION".into(), "proxy".into()),
                ("PROXY_COLLISION".into(), "proxy".into()),
                ("PROXY_ONLY".into(), "proxy".into()),
            ]),
            Some(&settings),
        )
        .expect("build configured launch");
        assert_eq!(
            launch.command.env,
            Some(HashMap::from_iter([
                ("COLLISION".into(), "explicit".into()),
                ("PROXY_COLLISION".into(), "proxy".into()),
                ("DIRECTORY_ONLY".into(), "directory".into()),
                ("PROXY_ONLY".into(), "proxy".into()),
                ("EXPLICIT_ONLY".into(), "explicit".into()),
            ]))
        );
    }

    #[test]
    fn codex_acp_launch_warns_when_directory_environment_is_unavailable() {
        let launch = build_launch("/arena".into(), None, HashMap::default(), None)
            .expect("build inherited-environment launch");
        assert_eq!(launch.command.path, PathBuf::from("codex-acp"));
        assert!(launch.command.args.is_empty());
        assert_eq!(launch.command.env, Some(HashMap::default()));
        assert!(
            launch
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("inherited environment"))
        );
        let launch = build_launch(
            "/arena".into(),
            Some(HashMap::default()),
            HashMap::default(),
            None,
        )
        .expect("build empty directory environment launch");
        assert!(launch.warning.is_none());
    }

    #[test]
    fn codex_acp_launch_rejects_registry_configuration() {
        let settings = CustomAgentServerSettings::Registry {
            env: HashMap::default(),
            default_mode: None,
            default_config_options: HashMap::default(),
            favorite_config_option_values: HashMap::default(),
        };
        let error = build_launch(
            "/arena".into(),
            Some(HashMap::default()),
            HashMap::default(),
            Some(&settings),
        )
        .err()
        .expect("Registry must not be launched as a custom command");
        assert!(
            error
                .to_string()
                .contains("ACP Registry installation is not supported")
        );
    }

    #[test]
    fn codex_acp_launch_preserves_custom_command_arguments_and_defaults() {
        let command = AgentServerCommand {
            path: "/installed/codex-acp".into(),
            args: vec!["--log-level".into(), "debug".into()],
            env: Some(HashMap::from_iter([(
                "CUSTOM_VALUE".into(),
                "value with spaces $literal".into(),
            )])),
        };
        let defaults = HashMap::from_iter([
            (
                "model".into(),
                AgentConfigOptionValue::ValueId("configured-model".into()),
            ),
            ("fast-mode".into(), AgentConfigOptionValue::Boolean(true)),
        ]);
        let settings = CustomAgentServerSettings::Custom {
            command: command.clone(),
            default_mode: Some("read-only".into()),
            default_config_options: defaults.clone(),
            favorite_config_option_values: HashMap::default(),
        };
        let launch = build_launch(
            "/arena".into(),
            Some(HashMap::default()),
            HashMap::default(),
            Some(&settings),
        )
        .expect("build custom launch");
        assert_eq!(launch.command, command);
        assert_eq!(
            launch.default_mode,
            Some(acp::SessionModeId::new("read-only"))
        );
        assert_eq!(launch.default_config_options, defaults);
    }

    #[gpui::test]
    async fn codex_acp_connect_rejects_invalid_arena_directories(cx: &mut TestAppContext) {
        init(cx);
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree("/arena", serde_json::json!({"file.txt": "regular file"}))
            .await;
        let project = Project::test(filesystem, [], cx).await;
        for directory in [
            None,
            Some(PathBuf::from("/missing")),
            Some(PathBuf::from("/arena/file.txt")),
        ] {
            let server = CodexAcpServer {
                working_directory: directory.clone(),
                warning: Arc::new(Mutex::new(None)),
            };
            let error = cx
                .update(|cx| {
                    let delegate = AgentServerDelegate::new(
                        project.read(cx).agent_server_store().clone(),
                        None,
                        None,
                    );
                    server.connect(delegate, project.clone(), cx)
                })
                .await
                .map(|_| ())
                .expect_err("invalid directory must fail before launching a process");
            if let Some(directory) = directory {
                assert!(error.to_string().contains(&directory.display().to_string()));
            } else {
                assert!(error.to_string().contains("no working directory"));
            }
        }
    }

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            release_channel::init(semver::Version::new(0, 1, 0), cx);
        });
    }

    async fn setup(
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Workspace>, &mut VisualTestContext) {
        init(cx);
        let filesystem = FakeFs::new(cx.executor());
        let project = Project::test(filesystem, [], cx).await;
        let (root, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = root.read_with(cx, |root, _| root.workspace().clone());
        (project, workspace, cx)
    }

    fn fake_tab(
        server: FakeAcpAgentServer,
        directory: &str,
        project: Entity<Project>,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<CodexAcpView> {
        cx.new_window_entity(|window, cx| {
            CodexAcpView::with_server(
                Rc::new(server),
                project,
                workspace.downgrade(),
                Some(directory.into()),
                Arc::new(Mutex::new(None)),
                window,
                cx,
            )
        })
    }

    fn thread(view: &Entity<CodexAcpView>, cx: &VisualTestContext) -> Entity<AcpThread> {
        view.read_with(cx, |view, cx| {
            view.conversation
                .read(cx)
                .root_thread(cx)
                .expect("connected root thread")
        })
    }

    struct PendingServer(Arc<std::sync::atomic::AtomicBool>);
    struct Cancelled(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Cancelled {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    impl AgentServer for PendingServer {
        fn logo(&self) -> IconName {
            IconName::AiOpenAi
        }
        fn agent_id(&self) -> project::AgentId {
            CODEX_ID.into()
        }
        fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
            self
        }
        fn connect(
            &self,
            _: AgentServerDelegate,
            _: Entity<Project>,
            cx: &mut App,
        ) -> Task<Result<Rc<dyn AgentConnection>>> {
            let cancelled = Cancelled(self.0.clone());
            cx.spawn(async move |_| {
                let _cancelled = cancelled;
                futures::future::pending().await
            })
        }
    }

    #[gpui::test]
    async fn codex_acp_closing_during_initialization_cancels_the_connection(
        cx: &mut TestAppContext,
    ) {
        let (project, workspace, cx) = setup(cx).await;
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let view = cx.new_window_entity(|window, cx| {
            CodexAcpView::with_server(
                Rc::new(PendingServer(cancelled.clone())),
                project,
                workspace.downgrade(),
                Some("/arena".into()),
                Arc::new(Mutex::new(None)),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        assert!(!cancelled.load(std::sync::atomic::Ordering::SeqCst));
        drop(view);
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[gpui::test]
    async fn codex_acp_tabs_keep_separate_connections_and_directories(cx: &mut TestAppContext) {
        let (project, workspace, cx) = setup(cx).await;
        let first_server = FakeAcpAgentServer::new();
        let second_server = FakeAcpAgentServer::new();
        let first = fake_tab(
            first_server.clone(),
            "/first",
            project.clone(),
            &workspace,
            cx,
        );
        let second = fake_tab(second_server, "/second", project, &workspace, cx);
        cx.run_until_parked();
        let first_thread = thread(&first, cx);
        let second_thread = thread(&second, cx);
        assert_ne!(first_thread.entity_id(), second_thread.entity_id());
        for (directory, thread) in [("/first", &first_thread), ("/second", &second_thread)] {
            let launch = build_launch(
                directory.into(),
                Some(HashMap::default()),
                HashMap::default(),
                None,
            )
            .expect("build actual launch options");
            thread.read_with(cx, |thread, _| {
                assert_eq!(
                    launch.options.working_directory.as_ref(),
                    thread
                        .work_dirs()
                        .and_then(|directories| directories.ordered_paths().next()),
                    "process cwd and session cwd must match",
                );
            });
        }
        first_thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread.work_dirs(),
                Some(&workspace::PathList::new(&["/first"]))
            );
        });
        second_thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread.work_dirs(),
                Some(&workspace::PathList::new(&["/second"]))
            );
        });
        assert_eq!(
            first.read_with(cx, |view, _| view.state()),
            CodexAcpState::Idle
        );
        assert_eq!(
            second.read_with(cx, |view, _| view.state()),
            CodexAcpState::Idle
        );
        first_server.fail_next_prompt();
        assert!(
            first_thread
                .update(cx, |thread, cx| thread.send_raw("fail", cx))
                .await
                .is_err()
        );
        cx.run_until_parked();
        assert_eq!(
            first.read_with(cx, |view, _| view.state()),
            CodexAcpState::Error
        );
        assert_eq!(
            second.read_with(cx, |view, _| view.state()),
            CodexAcpState::Idle
        );
        second_thread
            .update(cx, |thread, cx| thread.send_raw("succeed", cx))
            .await
            .expect("second tab still works");
        cx.run_until_parked();
        assert_eq!(
            second.read_with(cx, |view, _| view.state()),
            CodexAcpState::Completed
        );
    }

    #[gpui::test]
    async fn codex_acp_moves_keep_the_conversation_and_close_releases_session(
        cx: &mut TestAppContext,
    ) {
        let (project, workspace, cx) = setup(cx).await;
        let server = FakeAcpAgentServer::new();
        let closed = server.close_session_count();
        let view = fake_tab(server, "/arena", project.clone(), &workspace, cx);
        cx.run_until_parked();
        let conversation_id = view.read_with(cx, |view, _| view.conversation_id());
        let connection = view
            .read_with(cx, |view, cx| {
                let entry = view
                    ._connection_store
                    .read(cx)
                    .entry(&Agent::Custom {
                        id: CODEX_ID.into(),
                    })
                    .expect("connection entry");
                entry.read(cx).wait_for_connection()
            })
            .await
            .expect("tab must be connected");
        let weak_view = view.downgrade();
        let retained_thread = thread(&view, cx);
        let weak_store = view.read_with(cx, |view, _| view._connection_store.downgrade());
        let first_pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        first_pane.update_in(cx, |pane, window, cx| {
            pane.add_item(Box::new(view.clone()), true, false, None, window, cx);
        });
        let second_pane = cx.new_window_entity(|window, cx| {
            Pane::new(
                workspace.downgrade(),
                project,
                Default::default(),
                None,
                workspace::NewTerminal::default().boxed_clone(),
                false,
                window,
                cx,
            )
        });
        let item = first_pane
            .update_in(cx, |pane, window, cx| pane.take_active_item(window, cx))
            .expect("take tab for move");
        second_pane.update_in(cx, |pane, window, cx| {
            pane.add_item(item, true, false, None, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.conversation_id()),
            conversation_id
        );
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 0);
        let item = second_pane
            .update_in(cx, |pane, window, cx| pane.take_active_item(window, cx))
            .expect("close tab");
        drop(item);
        drop(view);
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(weak_view.upgrade().is_none(), "closed tab must be released");
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(
            weak_store.upgrade().is_none(),
            "review references must not retain the connection store"
        );
        drop(retained_thread);
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst) > 0);
        drop(connection);
    }

    #[gpui::test]
    async fn codex_acp_stale_completion_does_not_finish_a_new_turn(cx: &mut TestAppContext) {
        let (project, workspace, cx) = setup(cx).await;
        let view = fake_tab(FakeAcpAgentServer::new(), "/arena", project, &workspace, cx);
        cx.run_until_parked();
        let thread = thread(&view, cx);
        thread
            .update(cx, |thread, cx| thread.send_raw("hello", cx))
            .await
            .expect("send prompt");
        cx.run_until_parked();
        let generation = thread.read_with(cx, |thread, _| thread.activity_generation());
        view.update(cx, |view, cx| {
            view.state = CodexAcpState::Running;
            view.handle_thread_event(
                thread,
                &AcpThreadEvent::Stopped {
                    activity_generation: generation.saturating_add(1),
                    activity_duration: None,
                    stop_reason: Some(acp_v2::StopReason::EndTurn),
                },
                cx,
            );
            assert_eq!(view.state, CodexAcpState::Running);
        });
    }

    #[gpui::test]
    async fn codex_acp_review_tabs_keep_only_their_own_edits(cx: &mut TestAppContext) {
        init(cx);
        let filesystem = FakeFs::new(cx.executor());
        filesystem
            .insert_tree(
                "/arena",
                serde_json::json!({"first.txt": "before\n", "second.txt": "before\n"}),
            )
            .await;
        let project = Project::test(filesystem, [std::path::Path::new("/arena")], cx).await;
        let (root, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = root.read_with(cx, |root, _| root.workspace().clone());
        let first = fake_tab(
            FakeAcpAgentServer::new(),
            "/arena",
            project.clone(),
            &workspace,
            cx,
        );
        let second = fake_tab(
            FakeAcpAgentServer::new(),
            "/arena",
            project.clone(),
            &workspace,
            cx,
        );
        cx.run_until_parked();
        let first_thread = thread(&first, cx);
        let second_thread = thread(&second, cx);
        let first_log = first_thread.read_with(cx, |thread, _| thread.action_log().clone());
        let second_log = second_thread.read_with(cx, |thread, _| thread.action_log().clone());
        for (filename, action_log) in [("first.txt", &first_log), ("second.txt", &second_log)] {
            let path = project
                .read_with(cx, |project, cx| {
                    project.find_project_path(&format!("arena/{filename}"), cx)
                })
                .expect("project file");
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(path, cx))
                .await
                .expect("open buffer");
            cx.update(|_, cx| {
                action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
                buffer.update(cx, |buffer, cx| {
                    buffer
                        .edit([(0..6, "after")], None, cx)
                        .expect("edit buffer");
                });
                action_log.update(cx, |log, cx| log.buffer_edited(buffer, cx));
            });
        }
        cx.run_until_parked();
        assert_eq!(
            first_log.read_with(cx, |log, cx| log.changed_buffers(cx).count()),
            1
        );
        assert_eq!(
            second_log.read_with(cx, |log, cx| log.changed_buffers(cx).count()),
            1
        );
        let first_diff = cx.new_window_entity(|window, cx| {
            agent_ui::AgentDiffPane::new(first_thread, workspace.downgrade(), window, cx)
        });
        let second_diff = cx.new_window_entity(|window, cx| {
            agent_ui::AgentDiffPane::new(second_thread, workspace.downgrade(), window, cx)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_center(Box::new(first_diff.clone()), window, cx);
            workspace.add_item_to_center(Box::new(second_diff.clone()), window, cx);
            workspace.activate_item(&first_diff, true, true, window, cx);
        });
        cx.update(|window, cx| window.dispatch_action(agent_ui::KeepAll.boxed_clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            first_log.read_with(cx, |log, cx| log.changed_buffers(cx).count()),
            0
        );
        assert_eq!(
            second_log.read_with(cx, |log, cx| log.changed_buffers(cx).count()),
            1
        );
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&second_diff, true, true, window, cx);
        });
        cx.update(|window, cx| window.dispatch_action(agent_ui::KeepAll.boxed_clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            second_log.read_with(cx, |log, cx| log.changed_buffers(cx).count()),
            0
        );
    }

    #[gpui::test]
    async fn codex_acp_model_preferences_create_custom_settings(cx: &mut TestAppContext) {
        init(cx);
        let filesystem = FakeFs::new(cx.executor());
        let server = CodexAcpServer {
            working_directory: None,
            warning: Arc::new(Mutex::new(None)),
        };
        cx.update(|cx| {
            server.set_default_mode(
                Some(acp::SessionModeId::new("plan")),
                filesystem.clone(),
                cx,
            )
        });
        cx.run_until_parked();
        let contents = filesystem
            .load(paths::settings_file())
            .await
            .expect("saved settings");
        let contents: serde_json::Value =
            settings::parse_json_with_comments(&contents).expect("valid settings JSON");
        assert_eq!(contents["agent_servers"][CODEX_ID]["type"], "custom");
        assert_eq!(contents["agent_servers"][CODEX_ID]["command"], "codex-acp");
        assert_eq!(contents["agent_servers"][CODEX_ID]["default_mode"], "plan");
    }
}
