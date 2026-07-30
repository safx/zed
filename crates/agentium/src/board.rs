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
        board.tasks.push(BoardTask {
            id: Uuid::new_v4(),
            title: "archived one".to_string(),
            issues: Vec::new(),
            worktrees: Vec::new(),
            archived: true,
        });
        let id = board.tasks[0].id;
        assert_eq!(board.resolve_task("1").unwrap(), id);
        assert_eq!(board.resolve_task(&id.to_string()[..8]).unwrap(), id);
        assert_eq!(board.resolve_task("yyy").unwrap(), id);
        assert!(board.resolve_task("2").is_err());
        assert!(board.resolve_task("archived").is_err());
    }
}
