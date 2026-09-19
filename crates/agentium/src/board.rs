//! Task board model shared between the GUI app and the `agentium task` CLI.
//!
//! `board.json` (in `paths::data_dir()`) has exactly one writer at a time:
//! the running app applies `TaskCommand`s received over IPC and persists;
//! the CLI writes the file directly only when no app instance is listening.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use util::ResultExt as _;
use uuid::Uuid;

pub const BOARD_FORMAT_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Board {
    #[serde(default)]
    pub version: u32,
    // Vec order is priority order (top = highest).
    #[serde(default)]
    pub tasks: Vec<BoardTask>,
}

impl Default for Board {
    fn default() -> Self {
        Board {
            version: BOARD_FORMAT_VERSION,
            tasks: Vec::new(),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct BoardTask {
    pub id: Uuid,
    pub title: String,
    #[serde(default)]
    pub issues: Vec<IssueLink>,
    #[serde(default)]
    pub worktrees: Vec<PathBuf>,
    #[serde(default)]
    pub archived: bool,
    // Manually linked PRs. Each belongs to the arena(s) of this task whose
    // origin is the same repository; see `pr_ref_matches_remote`.
    #[serde(default)]
    pub prs: Vec<PrRef>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct IssueLink {
    #[serde(flatten)]
    pub reference: IssueRef,
    // Cached metadata so the UI stays readable when gh/bee are unavailable.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum IssueRef {
    #[serde(rename = "github")]
    GitHub {
        repo: String,
        number: u64,
    },
    Backlog {
        issue_key: String,
    },
}

impl IssueRef {
    pub fn short_label(&self) -> String {
        match self {
            IssueRef::GitHub { repo, number } => format!("{repo}#{number}"),
            IssueRef::Backlog { issue_key } => issue_key.clone(),
        }
    }
}

impl IssueLink {
    fn from_reference(reference: IssueRef) -> Self {
        let url = match &reference {
            IssueRef::GitHub { repo, number } => {
                Some(format!("https://github.com/{repo}/issues/{number}"))
            }
            IssueRef::Backlog { .. } => None,
        };
        IssueLink {
            reference,
            title: None,
            state: None,
            url,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PrStatus {
    Draft,
    Open,
    Merged,
    Closed,
    Conflicted,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PrProvider {
    GitHub,
    Backlog,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum PrRef {
    #[serde(rename = "github")]
    GitHub { repo: String, number: u32 },
    Backlog {
        web_host: String,
        project: String,
        repo: String,
        number: u32,
    },
}

impl PrRef {
    pub fn number(&self) -> u32 {
        match self {
            PrRef::GitHub { number, .. } => *number,
            PrRef::Backlog { number, .. } => *number,
        }
    }

    pub fn provider(&self) -> PrProvider {
        match self {
            PrRef::GitHub { .. } => PrProvider::GitHub,
            PrRef::Backlog { .. } => PrProvider::Backlog,
        }
    }

    pub fn short_label(&self) -> String {
        match self {
            PrRef::GitHub { repo, number } => format!("{repo}#{number}"),
            PrRef::Backlog { repo, number, .. } => format!("{repo}#{number}"),
        }
    }

    pub fn html_url(&self) -> String {
        match self {
            PrRef::GitHub { repo, number } => format!("https://github.com/{repo}/pull/{number}"),
            PrRef::Backlog {
                web_host,
                project,
                repo,
                number,
            } => format!("https://{web_host}/git/{project}/{repo}/pullRequests/{number}"),
        }
    }
}

/// A fully-resolved mutation, produced by the CLI (selectors and paths already
/// resolved against a read-only snapshot) and applied by whichever process owns
/// the board at that moment.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TaskCommand {
    New {
        id: Uuid,
        title: String,
        #[serde(default)]
        issues: Vec<IssueLink>,
        #[serde(default)]
        worktrees: Vec<PathBuf>,
    },
    AddIssue {
        task_id: Uuid,
        issue: IssueLink,
    },
    AddPr {
        task_id: Uuid,
        pr: PrRef,
    },
    AddArena {
        task_id: Uuid,
        path: PathBuf,
    },
    Archive {
        task_id: Uuid,
    },
}

impl Board {
    pub fn active_tasks(&self) -> impl Iterator<Item = &BoardTask> {
        self.tasks.iter().filter(|task| !task.archived)
    }

    /// Resolve a CLI task selector: 1-based index into the non-archived list
    /// (matching `agentium task list` output), a UUID prefix, or a unique
    /// case-insensitive title substring.
    pub fn resolve_task(&self, selector: &str) -> anyhow::Result<Uuid> {
        let active: Vec<&BoardTask> = self.active_tasks().collect();

        if let Ok(index) = selector.parse::<usize>() {
            if index >= 1 && index <= active.len() {
                return Ok(active[index - 1].id);
            }
            anyhow::bail!("task index {index} is out of range (1..={})", active.len());
        }

        let lowered = selector.to_lowercase();
        let by_uuid: Vec<&BoardTask> = active
            .iter()
            .copied()
            .filter(|task| task.id.to_string().starts_with(&lowered))
            .collect();
        if by_uuid.len() == 1 {
            return Ok(by_uuid[0].id);
        }

        let by_title: Vec<&BoardTask> = active
            .iter()
            .copied()
            .filter(|task| task.title.to_lowercase().contains(&lowered))
            .collect();
        match by_title.len() {
            1 => Ok(by_title[0].id),
            0 => anyhow::bail!(
                "no task matches {selector:?}; available:\n{}",
                format_candidates(&active)
            ),
            _ => anyhow::bail!(
                "{selector:?} is ambiguous; candidates:\n{}",
                format_candidates(&by_title)
            ),
        }
    }

    /// Find the tasks whose worktrees contain `path` (already canonicalized).
    pub fn tasks_containing_worktree(&self, path: &Path) -> Vec<&BoardTask> {
        self.active_tasks()
            .filter(|task| task.worktrees.iter().any(|w| w == path))
            .collect()
    }

    /// Find the tasks whose worktrees contain `path` or an ancestor of it
    /// (already canonicalized). Lets callers resolve a task from a
    /// subdirectory of a worktree, not just the worktree root itself.
    pub fn tasks_containing_path(&self, path: &Path) -> Vec<&BoardTask> {
        for ancestor in path.ancestors() {
            let matches = self.tasks_containing_worktree(ancestor);
            if !matches.is_empty() {
                return matches;
            }
        }
        Vec::new()
    }

    /// Resolve a task from an explicit `selector` (see `resolve_task`), or
    /// failing that, from `cwd` via `tasks_containing_path` (already
    /// canonicalized by the caller). `selector` takes priority: `cwd` is not
    /// consulted at all when it is present.
    pub fn resolve_target(&self, selector: Option<&str>, cwd: Option<&Path>) -> anyhow::Result<Uuid> {
        if let Some(selector) = selector {
            return self.resolve_task(selector);
        }
        let cwd = cwd.context("no task selector given and no current directory to resolve from")?;
        let active: Vec<&BoardTask> = self.active_tasks().collect();
        let containing = self.tasks_containing_path(cwd);
        match containing.len() {
            1 => Ok(containing[0].id),
            0 => anyhow::bail!(
                "current directory is not linked to any task; pass --task <TASK>. Tasks:\n{}",
                format_candidates(&active)
            ),
            _ => anyhow::bail!(
                "current directory belongs to multiple tasks; pass --task <TASK>. Tasks:\n{}",
                format_candidates(&active)
            ),
        }
    }

    /// Merge freshly-fetched title/state/url into every issue link matching
    /// `reference`, across all tasks (including archived ones, matching the
    /// existing in-place merge this replaces). `title` / `state` overwrite on
    /// any change; `url` only fills in when the existing value is `None`.
    /// Returns whether anything changed.
    pub fn merge_issue_metadata(
        &mut self,
        reference: &IssueRef,
        title: Option<&str>,
        state: Option<&str>,
        url: Option<&str>,
    ) -> bool {
        let mut changed = false;
        for task in &mut self.tasks {
            for issue in &mut task.issues {
                if issue.reference != *reference {
                    continue;
                }
                if let Some(title) = title {
                    if issue.title.as_deref() != Some(title) {
                        issue.title = Some(title.to_string());
                        changed = true;
                    }
                }
                if let Some(state) = state {
                    if issue.state.as_deref() != Some(state) {
                        issue.state = Some(state.to_string());
                        changed = true;
                    }
                }
                if issue.url.is_none() {
                    if let Some(url) = url {
                        issue.url = Some(url.to_string());
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    pub fn task_mut(&mut self, task_id: Uuid) -> anyhow::Result<&mut BoardTask> {
        self.tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            .with_context(|| format!("no task with id {task_id}"))
    }

    pub fn apply(&mut self, command: TaskCommand) -> anyhow::Result<()> {
        match command {
            TaskCommand::New {
                id,
                title,
                issues,
                worktrees,
            } => {
                // Idempotent against datagram duplication or a retried CLI run.
                if self.tasks.iter().any(|task| task.id == id) {
                    return Ok(());
                }
                self.tasks.push(BoardTask {
                    id,
                    title,
                    issues,
                    worktrees,
                    archived: false,
                    prs: Vec::new(),
                });
            }
            TaskCommand::AddIssue { task_id, issue } => {
                let task = self.task_mut(task_id)?;
                if !task
                    .issues
                    .iter()
                    .any(|existing| existing.reference == issue.reference)
                {
                    task.issues.push(issue);
                }
            }
            TaskCommand::AddPr { task_id, pr } => {
                let task = self.task_mut(task_id)?;
                if !task.prs.contains(&pr) {
                    task.prs.push(pr);
                }
            }
            TaskCommand::AddArena { task_id, path } => {
                let task = self.task_mut(task_id)?;
                if !task.worktrees.contains(&path) {
                    task.worktrees.push(path);
                }
            }
            TaskCommand::Archive { task_id } => {
                self.task_mut(task_id)?.archived = true;
            }
        }
        Ok(())
    }
}

fn format_candidates(tasks: &[&BoardTask]) -> String {
    if tasks.is_empty() {
        return "  (no tasks)".to_string();
    }
    tasks
        .iter()
        .map(|task| format!("  {}  {}", task.id, task.title))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn board_path() -> PathBuf {
    paths::data_dir().join("board.json")
}

pub fn load_board() -> Board {
    let path = board_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content)
            .map_err(|error| anyhow::anyhow!("Failed to parse {}: {error}", path.display()))
            .log_err()
            .unwrap_or_default(),
        Err(_) => Board::default(),
    }
}

pub fn write_board(board: &Board) -> anyhow::Result<()> {
    let path = board_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, serde_json::to_string_pretty(board)?)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Parse a user-supplied issue reference. Accepts pasted URLs first-class:
/// - `https://github.com/{owner}/{repo}/issues/{n}`
/// - `owner/repo#123`
/// - `https://{space}/view/{KEY}` (Backlog)
/// - `PROJ-198` (Backlog issue key)
pub fn parse_issue_ref(input: &str) -> anyhow::Result<IssueLink> {
    let input = input.trim();

    if let Some(rest) = input.strip_prefix("https://github.com/") {
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        if let [owner, repo, "issues", number] = parts[..] {
            let number: u64 = number
                .parse()
                .with_context(|| format!("invalid issue number in {input:?}"))?;
            return Ok(IssueLink {
                url: Some(format!("https://github.com/{owner}/{repo}/issues/{number}")),
                ..IssueLink::from_reference(IssueRef::GitHub {
                    repo: format!("{owner}/{repo}"),
                    number,
                })
            });
        }
        anyhow::bail!("unsupported GitHub URL (expected .../issues/<number>): {input}");
    }

    if input.starts_with("https://") || input.starts_with("http://") {
        let without_scheme = input.split("://").nth(1).unwrap_or_default();
        let mut segments = without_scheme.split('/');
        let host = segments.next().unwrap_or_default();
        if let (Some("view"), Some(key)) = (segments.next(), segments.next()) {
            let key = key.split(['?', '#']).next().unwrap_or(key);
            if is_backlog_issue_key(key) {
                return Ok(IssueLink {
                    url: Some(format!("https://{host}/view/{key}")),
                    ..IssueLink::from_reference(IssueRef::Backlog {
                        issue_key: key.to_string(),
                    })
                });
            }
        }
        anyhow::bail!("unsupported issue URL: {input}");
    }

    if let Some((repo, number)) = input.split_once('#') {
        if repo.split('/').count() == 2 && !repo.contains(char::is_whitespace) {
            let number: u64 = number
                .parse()
                .with_context(|| format!("invalid issue number in {input:?}"))?;
            return Ok(IssueLink::from_reference(IssueRef::GitHub {
                repo: repo.to_string(),
                number,
            }));
        }
    }

    if is_backlog_issue_key(input) {
        return Ok(IssueLink::from_reference(IssueRef::Backlog {
            issue_key: input.to_string(),
        }));
    }

    anyhow::bail!(
        "cannot parse issue reference {input:?} \
         (expected an issue URL, owner/repo#123, or a Backlog key like PROJ-198)"
    );
}

/// Parse a user-supplied PR reference. Accepts:
/// - `https://github.com/{owner}/{repo}/pull/{n}` (query string and trailing
///   path segments such as `/files` are ignored)
/// - `https://{host}/git/{project}/{repo}/pullRequests/{n}` (Backlog, any host)
/// - `owner/repo#N`
///
/// Bare numbers are deliberately not handled here: resolving them needs the
/// current repo's origin remote, which the CLI does separately.
pub fn parse_pr_ref(input: &str) -> anyhow::Result<PrRef> {
    let input = input.trim();
    const ACCEPTED_FORMS: &str = "expected a GitHub PR URL (https://github.com/owner/repo/pull/123), \
        a Backlog PR URL (https://space.backlog.jp/git/PROJECT/repo/pullRequests/123), \
        or owner/repo#123";

    if let Some(rest) = input.strip_prefix("https://github.com/") {
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        if parts.len() >= 4 && parts[2] == "pull" {
            let number: u32 = parts[3]
                .parse()
                .with_context(|| format!("invalid PR number in {input:?}"))?;
            return Ok(PrRef::GitHub {
                repo: format!("{}/{}", parts[0], parts[1]),
                number,
            });
        }
        anyhow::bail!("unsupported GitHub URL: {input} ({ACCEPTED_FORMS})");
    }

    if input.starts_with("https://") || input.starts_with("http://") {
        let without_scheme = input.split("://").nth(1).unwrap_or_default();
        let mut segments = without_scheme.split('/');
        let host = segments.next().unwrap_or_default();
        if segments.next() == Some("git") {
            if let (Some(project), Some(repo), Some("pullRequests"), Some(number)) = (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ) {
                let number = number.split(['?', '#']).next().unwrap_or(number);
                let number: u32 = number
                    .parse()
                    .with_context(|| format!("invalid PR number in {input:?}"))?;
                return Ok(PrRef::Backlog {
                    web_host: host.to_string(),
                    project: project.to_string(),
                    repo: repo.to_string(),
                    number,
                });
            }
        }
        anyhow::bail!("unsupported PR URL: {input} ({ACCEPTED_FORMS})");
    }

    if let Some((repo, number)) = input.split_once('#') {
        if repo.split('/').count() == 2 && !repo.contains(char::is_whitespace) {
            let number: u32 = number
                .parse()
                .with_context(|| format!("invalid PR number in {input:?}"))?;
            return Ok(PrRef::GitHub {
                repo: repo.to_string(),
                number,
            });
        }
    }

    anyhow::bail!("cannot parse PR reference {input:?} ({ACCEPTED_FORMS})");
}

pub(crate) fn is_backlog_issue_key(input: &str) -> bool {
    let Some((project, number)) = input.rsplit_once('-') else {
        return false;
    };
    !project.is_empty()
        && project
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        && project
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_board() -> Board {
        Board {
            version: 1,
            tasks: vec![BoardTask {
                id: Uuid::new_v4(),
                title: "XXXをYYYする".to_string(),
                issues: vec![
                    IssueLink {
                        reference: IssueRef::GitHub {
                            repo: "safx/zed".to_string(),
                            number: 12,
                        },
                        title: Some("A bug".to_string()),
                        state: Some("open".to_string()),
                        url: Some("https://github.com/safx/zed/issues/12".to_string()),
                    },
                    IssueLink::from_reference(IssueRef::Backlog {
                        issue_key: "PROJ-198".to_string(),
                    }),
                ],
                worktrees: vec![PathBuf::from("/tmp/example")],
                archived: false,
                prs: Vec::new(),
            }],
        }
    }

    #[test]
    fn board_round_trips_through_json_string() {
        let board = sample_board();
        let json = serde_json::to_string(&board).unwrap();
        let parsed: Board = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tasks[0].issues, board.tasks[0].issues);
        assert_eq!(parsed.tasks[0].id, board.tasks[0].id);
    }

    #[test]
    fn task_command_round_trips_through_value() {
        // The IPC listener parses the datagram into a serde_json::Value and
        // extracts the command via from_value, so cover that exact path.
        let command = TaskCommand::AddIssue {
            task_id: Uuid::new_v4(),
            issue: parse_issue_ref("https://example.backlog.jp/view/PROJ-198").unwrap(),
        };
        let value = serde_json::to_value(&command).unwrap();
        let parsed: TaskCommand = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, command);

        let json = serde_json::to_string(&command).unwrap();
        let parsed: TaskCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, command);
    }

    #[test]
    fn parse_issue_ref_accepts_documented_forms() {
        assert_eq!(
            parse_issue_ref("https://github.com/safx/zed/issues/12")
                .unwrap()
                .reference,
            IssueRef::GitHub {
                repo: "safx/zed".to_string(),
                number: 12
            }
        );
        assert_eq!(
            parse_issue_ref("safx/zed#12").unwrap().reference,
            IssueRef::GitHub {
                repo: "safx/zed".to_string(),
                number: 12
            }
        );
        let backlog = parse_issue_ref("https://example.backlog.jp/view/PROJ-198").unwrap();
        assert_eq!(
            backlog.reference,
            IssueRef::Backlog {
                issue_key: "PROJ-198".to_string()
            }
        );
        assert_eq!(
            backlog.url.as_deref(),
            Some("https://example.backlog.jp/view/PROJ-198")
        );
        assert_eq!(
            parse_issue_ref("PROJ-198").unwrap().reference,
            IssueRef::Backlog {
                issue_key: "PROJ-198".to_string()
            }
        );
        assert!(parse_issue_ref("not an issue").is_err());
        assert!(parse_issue_ref("lowercase-198").is_err());
    }

    #[test]
    fn resolve_task_by_index_uuid_prefix_and_title() {
        let mut board = sample_board();
        // A random v4 id can start with eight digits, which resolve_task would
        // read as a task index instead of a uuid prefix.
        board.tasks[0].id = Uuid::from_u128(0xabcdef01_2345_4678_8abc_def012345678);
        board.tasks.push(BoardTask {
            id: Uuid::new_v4(),
            title: "archived one".to_string(),
            issues: Vec::new(),
            worktrees: Vec::new(),
            archived: true,
            prs: Vec::new(),
        });
        let id = board.tasks[0].id;
        assert_eq!(board.resolve_task("1").unwrap(), id);
        assert_eq!(board.resolve_task(&id.to_string()[..8]).unwrap(), id);
        assert_eq!(board.resolve_task("yyy").unwrap(), id);
        assert!(board.resolve_task("2").is_err());
        assert!(board.resolve_task("archived").is_err());
    }

    #[test]
    fn parse_pr_ref_accepts_documented_forms() {
        assert_eq!(
            parse_pr_ref("https://github.com/safx/zed/pull/12").unwrap(),
            PrRef::GitHub {
                repo: "safx/zed".to_string(),
                number: 12
            }
        );
        assert_eq!(
            parse_pr_ref("https://github.com/safx/zed/pull/12/files").unwrap(),
            PrRef::GitHub {
                repo: "safx/zed".to_string(),
                number: 12
            }
        );
        assert_eq!(
            parse_pr_ref("https://github.com/safx/zed/pull/12?diff=split").unwrap(),
            PrRef::GitHub {
                repo: "safx/zed".to_string(),
                number: 12
            }
        );
        assert_eq!(
            parse_pr_ref("safx/zed#12").unwrap(),
            PrRef::GitHub {
                repo: "safx/zed".to_string(),
                number: 12
            }
        );
        assert_eq!(
            parse_pr_ref(
                "https://nulab.backlog.jp/git/BLG_AI_INTEGRATION/backlog-ai-agent/pullRequests/163"
            )
            .unwrap(),
            PrRef::Backlog {
                web_host: "nulab.backlog.jp".to_string(),
                project: "BLG_AI_INTEGRATION".to_string(),
                repo: "backlog-ai-agent".to_string(),
                number: 163,
            }
        );
    }

    #[test]
    fn parse_pr_ref_rejects_unsupported_forms() {
        assert!(parse_pr_ref("https://github.com/safx/zed/issues/12").is_err());
        assert!(parse_pr_ref("12").is_err());
        assert!(parse_pr_ref("not a pr").is_err());
    }

    #[test]
    fn task_command_add_pr_round_trips_through_value_and_string() {
        let command = TaskCommand::AddPr {
            task_id: Uuid::new_v4(),
            pr: parse_pr_ref("safx/zed#12").unwrap(),
        };
        let value = serde_json::to_value(&command).unwrap();
        let parsed: TaskCommand = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, command);

        let json = serde_json::to_string(&command).unwrap();
        let parsed: TaskCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, command);
    }

    #[test]
    fn apply_add_pr_is_idempotent() {
        let mut board = sample_board();
        let task_id = board.tasks[0].id;
        let pr = parse_pr_ref("safx/zed#12").unwrap();
        board
            .apply(TaskCommand::AddPr {
                task_id,
                pr: pr.clone(),
            })
            .unwrap();
        board
            .apply(TaskCommand::AddPr { task_id, pr })
            .unwrap();
        assert_eq!(board.tasks[0].prs.len(), 1);
    }

    #[test]
    fn pr_ref_deserializes_ignoring_legacy_cache_fields() {
        // Boards written by the first implementation carried cached title and
        // status next to the reference; they must still load.
        let json = r#"{"provider":"github","repo":"o/r","number":1,"title":"x","status":"open"}"#;
        let reference: PrRef = serde_json::from_str(json).unwrap();
        assert_eq!(
            reference,
            PrRef::GitHub {
                repo: "o/r".to_string(),
                number: 1
            }
        );
        let json = r#"{"provider":"backlog","web_host":"h.backlog.jp","project":"P","repo":"r","number":2,"title":"y","status":"closed"}"#;
        let reference: PrRef = serde_json::from_str(json).unwrap();
        assert_eq!(reference.number(), 2);
        assert_eq!(reference.provider(), PrProvider::Backlog);
    }

    #[test]
    fn pr_status_round_trips_as_lowercase_json() {
        assert_eq!(
            serde_json::to_string(&PrStatus::Conflicted).unwrap(),
            "\"conflicted\""
        );
        let parsed: PrStatus = serde_json::from_str("\"merged\"").unwrap();
        assert_eq!(parsed, PrStatus::Merged);
    }

    #[test]
    fn tasks_containing_path_resolves_subdirectories_of_a_worktree() {
        let board = sample_board();
        let worktree = &board.tasks[0].worktrees[0];
        let subdirectory = worktree.join("src");

        let by_worktree = board.tasks_containing_path(worktree);
        let by_subdirectory = board.tasks_containing_path(&subdirectory);
        assert_eq!(by_worktree.len(), 1);
        assert_eq!(by_subdirectory.len(), 1);
        assert_eq!(by_worktree[0].id, board.tasks[0].id);
        assert_eq!(by_subdirectory[0].id, board.tasks[0].id);

        assert!(
            board
                .tasks_containing_path(Path::new("/unrelated/path"))
                .is_empty()
        );
    }

    #[test]
    fn resolve_target_prefers_selector_over_cwd() {
        let board = sample_board();
        let id = board.tasks[0].id;
        // A selector that could not possibly match the cwd's task still wins,
        // proving cwd is not consulted at all when a selector is given.
        assert_eq!(
            board
                .resolve_target(Some("1"), Some(Path::new("/unrelated/path")))
                .unwrap(),
            id
        );
    }

    #[test]
    fn resolve_target_falls_back_to_cwd_subdirectory() {
        let board = sample_board();
        let id = board.tasks[0].id;
        let subdirectory = board.tasks[0].worktrees[0].join("src");
        assert_eq!(board.resolve_target(None, Some(&subdirectory)).unwrap(), id);
    }

    #[test]
    fn resolve_target_errors_on_no_match_multiple_matches_or_no_input() {
        let mut board = sample_board();
        let worktree = board.tasks[0].worktrees[0].clone();
        // No match: an unrelated cwd.
        let error = board
            .resolve_target(None, Some(Path::new("/unrelated/path")))
            .unwrap_err();
        assert!(error.to_string().contains(&board.tasks[0].title));

        // Multiple matches: a second task sharing the same worktree.
        board.tasks.push(BoardTask {
            id: Uuid::new_v4(),
            title: "second".to_string(),
            issues: Vec::new(),
            worktrees: vec![worktree.clone()],
            archived: false,
            prs: Vec::new(),
        });
        let error = board.resolve_target(None, Some(&worktree)).unwrap_err();
        assert!(error.to_string().contains("multiple tasks"));

        // Neither selector nor cwd.
        assert!(board.resolve_target(None, None).is_err());
    }

    #[test]
    fn merge_issue_metadata_updates_title_and_conditionally_url() {
        let mut board = sample_board();
        let reference = IssueRef::GitHub {
            repo: "safx/zed".to_string(),
            number: 12,
        };
        // Same title/state: no change.
        assert!(!board.merge_issue_metadata(&reference, Some("A bug"), Some("open"), None));
        // Different title: changes and updates.
        assert!(board.merge_issue_metadata(&reference, Some("A different bug"), None, None));
        assert_eq!(
            board.tasks[0].issues[0].title.as_deref(),
            Some("A different bug")
        );
        // Existing url is Some, so a new url must not overwrite it.
        let before_url = board.tasks[0].issues[0].url.clone();
        assert!(!board.merge_issue_metadata(&reference, None, None, Some("https://example.com")));
        assert_eq!(board.tasks[0].issues[0].url, before_url);

        // url fills in only when previously None.
        let backlog_reference = IssueRef::Backlog {
            issue_key: "PROJ-198".to_string(),
        };
        assert!(board.tasks[0].issues[1].url.is_none());
        assert!(board.merge_issue_metadata(&backlog_reference, None, None, Some("https://x")));
        assert_eq!(
            board.tasks[0].issues[1].url.as_deref(),
            Some("https://x")
        );
    }

    #[test]
    fn merge_issue_metadata_applies_to_every_matching_task() {
        let mut board = sample_board();
        let reference = IssueRef::GitHub {
            repo: "safx/zed".to_string(),
            number: 12,
        };
        board.tasks.push(BoardTask {
            id: Uuid::new_v4(),
            title: "second".to_string(),
            issues: vec![IssueLink::from_reference(reference.clone())],
            worktrees: Vec::new(),
            archived: false,
            prs: Vec::new(),
        });
        assert!(board.merge_issue_metadata(&reference, Some("shared title"), None, None));
        assert_eq!(
            board.tasks[0].issues[0].title.as_deref(),
            Some("shared title")
        );
        assert_eq!(
            board.tasks[1].issues[0].title.as_deref(),
            Some("shared title")
        );
    }
}
