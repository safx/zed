use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use acp_thread::{AgentSessionInfo, AgentSessionList, AgentSessionListRequest, AuthRequired};
use agent_servers::{AgentServer, CODEX_ID};
use agent_ui::{Agent, AgentConnectionStore};
use anyhow::{Context as _, Result};
use gpui::{
    App, AsyncApp, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    SharedString, Subscription, Task, WeakEntity, Window, div, prelude::*, rems,
};
use parking_lot::Mutex;
use picker::{Picker, PickerDelegate};
use project::Project;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::ModalView;

use crate::{
    AgentiumApp,
    arena::Arena,
    codex_acp_persistence::{CodexAcpPersistence, SavedCodexTab},
    codex_acp_view::CodexAcpView,
};

pub(crate) struct OpenConversation {
    pub pane: Entity<workspace::Pane>,
    pub view: Entity<CodexAcpView>,
}

pub(crate) fn resume_target(
    arenas: &[Entity<Arena>],
    origin: &Entity<Arena>,
    directory: &Path,
    session_id: &str,
    persistence: &CodexAcpPersistence,
    cx: &App,
) -> Result<Option<OpenConversation>> {
    anyhow::ensure!(
        arenas.contains(origin),
        "The originating Arena is no longer open"
    );
    anyhow::ensure!(
        origin.read(cx).working_directory.as_deref() == Some(directory),
        "The originating Arena directory has changed"
    );
    let mut existing = None;
    let mut existing_id = None;
    for arena in arenas {
        for pane in arena.read(cx).center.panes() {
            for view in pane.read(cx).items_of_type::<CodexAcpView>() {
                if let Some(saved) = view
                    .read(cx)
                    .saved_tab()
                    .filter(|saved| saved.session_id == session_id)
                {
                    anyhow::ensure!(
                        arena == origin && existing.is_none(),
                        "This conversation is already open in Arena {}",
                        arena.read(cx).name
                    );
                    existing_id = Some(saved.id);
                    existing = Some(OpenConversation {
                        pane: pane.clone(),
                        view,
                    });
                }
            }
        }
    }
    check_saved_conflicts(
        directory,
        existing_id,
        &persistence.session_records(session_id),
    )?;
    Ok(existing)
}

pub(crate) fn check_saved_conflicts(
    directory: &Path,
    existing_id: Option<uuid::Uuid>,
    records: &[(PathBuf, SavedCodexTab)],
) -> Result<()> {
    for (saved_directory, saved) in records {
        anyhow::ensure!(
            saved_directory == directory && existing_id == Some(saved.id),
            "This conversation is already saved for {}. Open its Arena to restore it.",
            saved_directory.display()
        );
    }
    Ok(())
}

fn canonical_or_original(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

async fn collect_sessions(
    list: Rc<dyn AgentSessionList>,
    directory: PathBuf,
    cx: &mut AsyncApp,
) -> Result<Vec<AgentSessionInfo>> {
    let canonical = cx
        .background_executor()
        .spawn({
            let directory = directory.clone();
            async move { canonical_or_original(directory) }
        })
        .await;
    let mut directories = vec![directory.clone()];
    if canonical != directory {
        directories.push(canonical.clone());
    }
    let mut sessions = HashMap::new();
    for directory in directories {
        let mut cursor = None;
        let mut cursors = HashSet::new();
        loop {
            let request = AgentSessionListRequest {
                cwd: Some(directory.clone()),
                cursor,
                ..Default::default()
            };
            let response = cx.update(|cx| list.list_sessions(request, cx)).await?;
            let page = cx
                .background_executor()
                .spawn({
                    let canonical = canonical.clone();
                    async move {
                        response
                            .sessions
                            .into_iter()
                            .filter(|session| {
                                session
                                    .work_dirs
                                    .as_ref()
                                    .and_then(|paths| paths.ordered_paths().next())
                                    .is_some_and(|cwd| {
                                        canonical_or_original(cwd.clone()) == canonical
                                    })
                            })
                            .collect::<Vec<_>>()
                    }
                })
                .await;
            for session in page {
                sessions
                    .entry(session.session_id.clone())
                    .or_insert(session);
            }
            match response.next_cursor {
                Some(next) => {
                    anyhow::ensure!(
                        cursors.insert(next.clone()),
                        "The agent returned a repeated history cursor"
                    );
                    cursor = Some(next);
                }
                None => break,
            }
        }
    }
    let mut sessions = sessions.into_values().collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| session_title(left).cmp(&session_title(right)))
            .then_with(|| {
                left.session_id
                    .to_string()
                    .cmp(&right.session_id.to_string())
            })
    });
    Ok(sessions)
}

fn session_title(session: &AgentSessionInfo) -> String {
    session
        .title
        .as_ref()
        .filter(|title| !title.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            format!(
                "Conversation {}",
                session
                    .session_id
                    .to_string()
                    .chars()
                    .take(8)
                    .collect::<String>()
            )
        })
}

pub(crate) struct CodexResume {
    picker: Entity<Picker<ResumeDelegate>>,
    project: Entity<Project>,
    directory: PathBuf,
    server: Rc<dyn AgentServer>,
    connection_store: Option<Entity<AgentConnectionStore>>,
    warning: Arc<Mutex<Option<String>>>,
    task: Task<()>,
    _subscription: Subscription,
}

impl CodexResume {
    pub(crate) fn new(
        arena: WeakEntity<Arena>,
        directory: PathBuf,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let warning = Arc::new(Mutex::new(None));
        let server = CodexAcpView::server(Some(directory.clone()), warning.clone());
        Self::with_server(arena, directory, project, server, warning, window, cx)
    }

    fn with_server(
        arena: WeakEntity<Arena>,
        directory: PathBuf,
        project: Entity<Project>,
        server: Rc<dyn AgentServer>,
        warning: Arc<Mutex<Option<String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let owner = cx.weak_entity();
        let delegate = ResumeDelegate {
            owner,
            arena,
            directory: directory.clone(),
            sessions: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            status: Some("Connecting to Codex…".into()),
            failed: false,
        };
        // A stashed picker would keep the history connection and pending request alive.
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(rems(34.))
                .reopenable(false, cx)
        });
        let subscription =
            cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent));
        let mut this = Self {
            picker,
            project,
            directory,
            server,
            connection_store: None,
            warning,
            task: Task::ready(()),
            _subscription: subscription,
        };
        this.load(window, cx);
        this
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.task = Task::ready(());
        self.connection_store = None;
        *self.warning.lock() = None;
        self.picker.update(cx, |picker, cx| {
            picker.delegate.sessions.clear();
            picker.delegate.matches.clear();
            picker.delegate.status = Some("Connecting to Codex…".into());
            picker.delegate.failed = false;
            cx.notify();
        });
        let store = cx.new(|cx| AgentConnectionStore::new_isolated(self.project.clone(), cx));
        let connection = store.update(cx, |store, cx| {
            store
                .request_connection(
                    Agent::Custom {
                        id: CODEX_ID.into(),
                    },
                    self.server.clone(),
                    cx,
                )
                .read(cx)
                .wait_for_connection()
        });
        self.connection_store = Some(store);
        let directory = self.directory.clone();
        self.task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let state = connection.await.map_err(anyhow::Error::new)?;
                anyhow::ensure!(
                    state.connection.supports_load_session(),
                    "This agent does not support restoring conversation history"
                );
                let list = cx
                    .update(|_, cx| state.connection.session_list(cx))?
                    .context("This agent does not support listing conversations")?;
                this.update(cx, |this, cx| {
                    this.picker.update(cx, |picker, cx| {
                        picker.delegate.status = Some("Loading conversations…".into());
                        cx.notify();
                    });
                })?;
                collect_sessions(list, directory, cx).await
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                this.picker.update(cx, |picker, cx| {
                    match result {
                        Ok(sessions) => {
                            picker.delegate.sessions = sessions;
                            picker.delegate.status = None;
                        }
                        Err(error) => {
                            log::error!("Could not list Codex conversations: {error:#}");
                            picker.delegate.status =
                                Some(if error.downcast_ref::<AuthRequired>().is_some() {
                                    "Sign in using a new Codex (ACP) tab, then retry here.".into()
                                } else {
                                    format!("Could not load conversations: {error:#}").into()
                                });
                            picker.delegate.failed = true;
                        }
                    }
                    picker.update_matches(picker.query(cx), window, cx);
                    cx.notify();
                });
                cx.notify();
            })
            .log_err();
        });
    }
}

impl ModalView for CodexResume {}
impl EventEmitter<DismissEvent> for CodexResume {}
impl Focusable for CodexResume {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}
impl Render for CodexResume {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone()).when_some(
            self.warning.lock().clone(),
            |element, warning| {
                element.child(
                    div()
                        .p_2()
                        .text_sm()
                        .text_color(cx.theme().status().warning)
                        .child(warning),
                )
            },
        )
    }
}

struct ResumeDelegate {
    owner: WeakEntity<CodexResume>,
    arena: WeakEntity<Arena>,
    directory: PathBuf,
    sessions: Vec<AgentSessionInfo>,
    matches: Vec<usize>,
    selected: usize,
    status: Option<SharedString>,
    failed: bool,
}

impl PickerDelegate for ResumeDelegate {
    type ListItem = ListItem;
    fn name() -> &'static str {
        "Codex conversations"
    }
    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Search Codex conversations…".into()
    }
    fn match_count(&self) -> usize {
        self.matches.len()
    }
    fn selected_index(&self) -> usize {
        self.selected
    }
    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = index;
    }
    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                session_title(session).to_lowercase().contains(&query)
                    || session
                        .session_id
                        .to_string()
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        cx.notify();
        Task::ready(())
    }
    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(session) = self
            .matches
            .get(self.selected)
            .and_then(|index| self.sessions.get(*index))
        else {
            return;
        };
        let result = (|| {
            let arena = self
                .arena
                .upgrade()
                .context("The originating Arena is no longer open")?;
            let application = window
                .root::<AgentiumApp>()
                .flatten()
                .context("The Agentium window is no longer available")?;
            application.update(cx, |application, cx| {
                application.resume_codex_conversation(
                    &arena,
                    &self.directory,
                    &session.session_id.to_string(),
                    window,
                    cx,
                )
            })
        })();
        match result {
            Ok(()) => cx.emit(DismissEvent),
            Err(error) => {
                self.status = Some(format!("{error:#}").into());
                self.failed = true;
                cx.notify();
            }
        }
    }
    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }
    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some(self.status.clone().unwrap_or_else(|| {
            if self.sessions.is_empty() {
                "No Codex conversations in this worktree".into()
            } else {
                "No matching conversations".into()
            }
        }))
    }
    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<ListItem> {
        let session = self.sessions.get(*self.matches.get(index)?)?;
        let updated = session
            .updated_at
            .map(|date| {
                date.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "Unknown update time".into());
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex().child(Label::new(session_title(session))).child(
                        Label::new(updated)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                ),
        )
    }
    fn render_footer(&self, _: &mut Window, _: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        let owner = self.owner.clone();
        Some(
            v_flex()
                .p_2()
                .gap_2()
                .when(self.failed && !self.matches.is_empty(), |element| {
                    element.child(
                        Label::new(self.status.clone().unwrap_or_default())
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                })
                .child(
                    Label::new(self.directory.to_string_lossy().into_owned())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .when(self.failed, |element| {
                    element.child(Button::new("retry-codex-history", "Retry").on_click(
                        move |_, window, cx| {
                            // Retrying updates the picker, so release its current event-handler borrow first.
                            let owner = owner.clone();
                            window.defer(cx, move |window, cx| {
                                owner
                                    .update(cx, |owner, cx| owner.load(window, cx))
                                    .log_err();
                            });
                        },
                    ))
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::{AcpThread, AgentConnection, AgentSessionListResponse};
    use agent_client_protocol::schema::v1 as acp;
    use agent_servers::AgentServerDelegate;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use std::{
        any::Any,
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use workspace::{MultiWorkspace, PathList};

    #[derive(Default)]
    struct TestList {
        pages: Mutex<VecDeque<Result<AgentSessionListResponse>>>,
        requests: Mutex<Vec<AgentSessionListRequest>>,
        pending: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
    }
    impl AgentSessionList for TestList {
        fn list_sessions(
            &self,
            request: AgentSessionListRequest,
            cx: &mut App,
        ) -> Task<Result<AgentSessionListResponse>> {
            self.requests.lock().push(request);
            if let Some(cancelled) = self.pending.lock().clone() {
                let guard = CancellationGuard(cancelled);
                return cx.spawn(async move |_| {
                    let _guard = guard;
                    futures::future::pending().await
                });
            }
            Task::ready(
                self.pages
                    .lock()
                    .pop_front()
                    .expect("expected history request"),
            )
        }
        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }
    struct CancellationGuard(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for CancellationGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    fn page(
        sessions: Vec<AgentSessionInfo>,
        next: Option<&str>,
    ) -> Result<AgentSessionListResponse> {
        Ok(AgentSessionListResponse {
            sessions,
            next_cursor: next.map(ToOwned::to_owned),
            meta: None,
        })
    }
    fn session(id: &str, cwd: &str, extra: Option<&str>, updated: Option<i64>) -> AgentSessionInfo {
        let mut session = AgentSessionInfo::new(id.to_owned());
        let mut paths = vec![PathBuf::from(cwd)];
        if let Some(extra) = extra {
            paths.push(extra.into());
        }
        session.work_dirs = Some(PathList::new(&paths));
        session.updated_at =
            updated.and_then(|updated| chrono::DateTime::from_timestamp(updated, 0));
        session
    }

    #[gpui::test]
    async fn resume_history_handles_empty_pages_cwd_order_duplicates_and_dates(
        cx: &mut TestAppContext,
    ) {
        let list = Rc::new(TestList::default());
        list.pages.lock().extend([
            page(Vec::new(), Some("page-2")),
            page(
                vec![
                    session("older", "/missing-z", Some("/missing-a"), Some(1)),
                    session("wrong-cwd", "/missing-other", Some("/missing-z"), Some(3)),
                ],
                Some("page-3"),
            ),
            page(
                vec![
                    session("newer", "/missing-z", None, Some(2)),
                    session("older", "/missing-z", None, Some(1)),
                    session("undated", "/missing-z", None, None),
                    AgentSessionInfo::new("no-cwd"),
                ],
                None,
            ),
        ]);
        let sessions = cx
            .update(|cx| {
                cx.spawn({
                    let list = list.clone();
                    async move |cx| collect_sessions(list, "/missing-z".into(), cx).await
                })
            })
            .await
            .expect("history");
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.session_id.to_string())
                .collect::<Vec<_>>(),
            vec!["newer", "older", "undated"]
        );
        assert_eq!(
            list.requests
                .lock()
                .iter()
                .map(|request| request.cursor.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("page-2"), Some("page-3")]
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn resume_history_merges_raw_and_canonical_paths(cx: &mut TestAppContext) {
        let temporary = tempfile::tempdir().expect("paths");
        let canonical = temporary.path().join("worktree");
        let alias = temporary.path().join("alias");
        std::fs::create_dir(&canonical).expect("worktree");
        std::os::unix::fs::symlink(&canonical, &alias).expect("alias");
        let list = Rc::new(TestList::default());
        list.pages.lock().extend([
            page(
                vec![session("raw", &alias.to_string_lossy(), None, None)],
                None,
            ),
            page(
                vec![
                    session("canonical", &canonical.to_string_lossy(), None, None),
                    session("raw", &alias.to_string_lossy(), None, None),
                ],
                None,
            ),
        ]);
        let sessions = cx
            .update(|cx| {
                cx.spawn({
                    let list = list.clone();
                    let alias = alias.clone();
                    async move |cx| collect_sessions(list, alias, cx).await
                })
            })
            .await
            .expect("history");
        assert_eq!(sessions.len(), 2);
        let requests = list.requests.lock();
        assert_eq!(
            requests.first().and_then(|request| request.cwd.as_ref()),
            Some(&alias)
        );
        assert_eq!(
            requests.get(1).and_then(|request| request.cwd.as_ref()),
            Some(&std::fs::canonicalize(canonical).expect("canonical path"))
        );
        assert!(requests.iter().all(|request| request.cursor.is_none()));
    }

    #[gpui::test]
    async fn resume_history_rejects_cycles_and_propagates_errors(cx: &mut TestAppContext) {
        for pages in [
            vec![
                page(Vec::new(), Some("cycle")),
                page(Vec::new(), Some("cycle")),
            ],
            vec![Err(anyhow::anyhow!("History unavailable"))],
        ] {
            let list = Rc::new(TestList::default());
            list.pages.lock().extend(pages);
            let result = cx
                .update(|cx| {
                    cx.spawn(async move |cx| collect_sessions(list, "/missing".into(), cx).await)
                })
                .await;
            assert!(result.is_err());
        }
    }

    #[derive(Clone)]
    struct TestServer {
        list: Arc<TestList>,
        load_supported: bool,
        list_supported: bool,
        connects: Arc<AtomicUsize>,
        new_sessions: Arc<AtomicUsize>,
    }
    impl AgentServer for TestServer {
        fn logo(&self) -> IconName {
            IconName::AiOpenAi
        }
        fn agent_id(&self) -> project::AgentId {
            CODEX_ID.into()
        }
        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
        fn connect(
            &self,
            _: AgentServerDelegate,
            _: Entity<Project>,
            _: &mut App,
        ) -> Task<Result<Rc<dyn AgentConnection>>> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Task::ready(Ok(Rc::new(self.clone())))
        }
    }
    impl AgentConnection for TestServer {
        fn agent_id(&self) -> project::AgentId {
            CODEX_ID.into()
        }
        fn telemetry_id(&self) -> SharedString {
            "resume-test".into()
        }
        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
        fn supports_load_session(&self) -> bool {
            self.load_supported
        }
        fn session_list(&self, _: &mut App) -> Option<Rc<dyn AgentSessionList>> {
            self.list_supported
                .then(|| Rc::new(SharedTestList(self.list.clone())) as Rc<dyn AgentSessionList>)
        }
        fn new_session(
            self: Rc<Self>,
            _: Entity<Project>,
            _: PathList,
            _: &mut App,
        ) -> Task<Result<Entity<AcpThread>>> {
            self.new_sessions.fetch_add(1, Ordering::SeqCst);
            Task::ready(Err(anyhow::anyhow!(
                "History picker must not create sessions"
            )))
        }
        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }
        fn authenticate(&self, _: acp::AuthMethodId, _: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }
        fn prompt(&self, _: acp::PromptRequest, _: &mut App) -> Task<Result<acp::PromptResponse>> {
            Task::ready(Err(anyhow::anyhow!("History picker must not send prompts")))
        }
        fn cancel(&self, _: &acp::SessionId, _: &mut App) {}
    }
    struct SharedTestList(Arc<TestList>);
    impl AgentSessionList for SharedTestList {
        fn list_sessions(
            &self,
            request: AgentSessionListRequest,
            cx: &mut App,
        ) -> Task<Result<AgentSessionListResponse>> {
            self.0.list_sessions(request, cx)
        }
        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }
    async fn setup(cx: &mut TestAppContext) -> (Entity<Project>, &mut VisualTestContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(db::AppDatabase::test_new());
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            release_channel::init(semver::Version::new(0, 1, 0), cx);
        });
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let (_, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        (project, cx)
    }
    fn server() -> TestServer {
        TestServer {
            list: Arc::new(TestList::default()),
            load_supported: true,
            list_supported: true,
            connects: Arc::new(AtomicUsize::new(0)),
            new_sessions: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn dialog(
        server: TestServer,
        project: Entity<Project>,
        cx: &mut VisualTestContext,
    ) -> Entity<CodexResume> {
        cx.new_window_entity(|window, cx| {
            CodexResume::with_server(
                WeakEntity::new_invalid(),
                "/missing-z".into(),
                project,
                Rc::new(server),
                Arc::new(Mutex::new(None)),
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    async fn resume_target_prevents_duplicates_and_preserves_tab_names(cx: &mut TestAppContext) {
        let (project, cx) = setup(cx).await;
        let temporary = tempfile::tempdir().expect("persistence");
        let persistence = cx.new(|_| CodexAcpPersistence::new(temporary.path().join("tabs.json")));
        let arena = cx.new_window_entity(|window, cx| {
            Arena::test_empty(
                "Origin".into(),
                project.clone(),
                "/missing-z".into(),
                window,
                cx,
            )
        });
        let other = cx.new_window_entity(|window, cx| {
            Arena::test_empty("Other".into(), project, "/missing-other".into(), window, cx)
        });
        let arenas = vec![arena.clone(), other.clone()];
        cx.update(|_, cx| {
            assert!(
                resume_target(
                    &arenas,
                    &arena,
                    Path::new("/missing-z"),
                    "selected",
                    persistence.read(cx),
                    cx
                )
                .expect("new session")
                .is_none()
            );
        });
        arena
            .update_in(cx, |arena, window, cx| {
                arena.resume_codex_tab("selected".into(), window, cx)
            })
            .expect("resume tab");
        let first = arena
            .read_with(cx, |arena, cx| arena.codex_tabs(cx))
            .pop()
            .expect("saved ID");
        assert_eq!(first.session_id, "selected");
        assert_eq!(first.title, "Codex ACP");
        persistence.update(cx, |store, cx| {
            store.replace("/missing-z".into(), vec![first.clone()], cx)
        });
        cx.update(|_, cx| {
            let target = resume_target(
                &arenas,
                &arena,
                Path::new("/missing-z"),
                "selected",
                persistence.read(cx),
                cx,
            )
            .expect("existing session")
            .expect("existing tab");
            assert_eq!(target.view.read(cx).saved_tab(), Some(first.clone()));
            assert!(
                resume_target(
                    &arenas,
                    &other,
                    Path::new("/missing-other"),
                    "selected",
                    persistence.read(cx),
                    cx
                )
                .is_err()
            );
            assert!(
                resume_target(
                    &[],
                    &arena,
                    Path::new("/missing-z"),
                    "selected",
                    persistence.read(cx),
                    cx
                )
                .is_err()
            );
            assert!(
                resume_target(
                    &arenas,
                    &arena,
                    Path::new("/changed"),
                    "selected",
                    persistence.read(cx),
                    cx
                )
                .is_err()
            );
        });
        arena
            .update_in(cx, |arena, window, cx| {
                arena.resume_codex_tab("second".into(), window, cx)
            })
            .expect("second tab");
        let tabs = arena.read_with(cx, |arena, cx| arena.codex_tabs(cx));
        assert_eq!(tabs.len(), 2);
        assert!(
            tabs.iter()
                .any(|tab| tab.title == "Codex ACP 2" && tab.session_id == "second")
        );
        cx.run_until_parked();
        assert_eq!(
            arena.read_with(cx, |arena, cx| arena.codex_tabs(cx)).len(),
            2
        );
    }

    #[gpui::test]
    async fn resume_picker_searches_titles_and_ids_without_creating_sessions(
        cx: &mut TestAppContext,
    ) {
        let (project, cx) = setup(cx).await;
        let server = server();
        let mut named = session("named-id", "/missing-z", None, Some(2));
        named.title = Some("Fix regression".into());
        server.list.pages.lock().push_back(page(
            vec![named, session("other-id", "/missing-z", None, Some(1))],
            None,
        ));
        let dialog = dialog(server.clone(), project, cx);
        cx.run_until_parked();
        dialog.update_in(cx, |dialog, window, cx| {
            dialog.picker.update(cx, |picker, cx| {
                assert!(picker.delegate.status.is_none());
                assert_eq!(picker.delegate.matches.len(), 2);
                picker.update_matches("REGRESSION".into(), window, cx);
                assert_eq!(picker.delegate.matches.len(), 1);
                picker.update_matches("other-id".into(), window, cx);
                assert_eq!(picker.delegate.matches, vec![1]);
                picker.update_matches("absent".into(), window, cx);
                assert_eq!(
                    picker.delegate.no_matches_text(window, cx),
                    Some("No matching conversations".into())
                );
            })
        });
        assert_eq!(server.new_sessions.load(Ordering::SeqCst), 0);
    }

    #[gpui::test]
    async fn resume_picker_reports_auth_and_retries_with_a_fresh_connection(
        cx: &mut TestAppContext,
    ) {
        let (project, cx) = setup(cx).await;
        let server = server();
        server
            .list
            .pages
            .lock()
            .extend([Err(AuthRequired::new().into()), page(Vec::new(), None)]);
        let dialog = dialog(server.clone(), project, cx);
        cx.run_until_parked();
        assert!(dialog.read_with(cx, |dialog, cx| {
            dialog
                .picker
                .read(cx)
                .delegate
                .status
                .as_ref()
                .is_some_and(|status| status.contains("Sign in"))
        }));
        dialog.update_in(cx, |dialog, window, cx| dialog.load(window, cx));
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, cx| {
            let delegate = &dialog.picker.read(cx).delegate;
            assert!(delegate.status.is_none());
            assert!(delegate.sessions.is_empty());
        });
        assert_eq!(server.connects.load(Ordering::SeqCst), 2);
    }

    #[gpui::test]
    async fn resume_picker_reports_missing_capabilities(cx: &mut TestAppContext) {
        let (project, cx) = setup(cx).await;
        for load in [false, true] {
            let mut server = server();
            server.load_supported = load;
            server.list_supported = false;
            let dialog = dialog(server, project.clone(), cx);
            cx.run_until_parked();
            assert!(dialog.read_with(cx, |dialog, cx| dialog.picker.read(cx).delegate.failed));
        }
    }

    #[gpui::test]
    async fn resume_picker_close_and_retry_cancel_pending_history(cx: &mut TestAppContext) {
        let (project, cx) = setup(cx).await;
        for retry in [false, true] {
            let server = server();
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            *server.list.pending.lock() = Some(cancelled.clone());
            let dialog = dialog(server.clone(), project.clone(), cx);
            cx.run_until_parked();
            assert!(!cancelled.load(Ordering::SeqCst));
            if retry {
                *server.list.pending.lock() = None;
                server
                    .list
                    .pages
                    .lock()
                    .push_back(page(vec![session("fresh", "/missing-z", None, None)], None));
                dialog.update_in(cx, |dialog, window, cx| dialog.load(window, cx));
                cx.run_until_parked();
                assert_eq!(
                    dialog.read_with(cx, |dialog, cx| dialog
                        .picker
                        .read(cx)
                        .delegate
                        .sessions
                        .len()),
                    1
                );
            }
            drop(dialog);
            cx.update(|_, _| {});
            cx.run_until_parked();
            assert!(cancelled.load(Ordering::SeqCst));
        }
    }

    #[gpui::test]
    async fn resume_modal_cancel_releases_pending_history(cx: &mut TestAppContext) {
        let (project, cx) = setup(cx).await;
        let server = server();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *server.list.pending.lock() = Some(cancelled.clone());
        let workspace = cx.update(|window, cx| {
            window
                .root::<MultiWorkspace>()
                .flatten()
                .expect("workspace window")
                .read(cx)
                .workspace()
                .clone()
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                CodexResume::with_server(
                    WeakEntity::new_invalid(),
                    "/missing-z".into(),
                    project,
                    Rc::new(server),
                    Arc::new(Mutex::new(None)),
                    window,
                    cx,
                )
            });
        });
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<CodexResume>(cx).is_some()
        }));
        cx.dispatch_action(menu::Cancel);
        cx.run_until_parked();
        assert!(workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<CodexResume>(cx).is_none()
        }));
        assert!(cancelled.load(Ordering::SeqCst));
    }
}
