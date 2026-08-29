use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{prelude::*, *};
use language::{Buffer, BufferEvent, LanguageRegistry, Point};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use project::{Project, ProjectEntryId, ProjectPath};
use regex::Regex;
use settings::{Settings as _, update_settings_file};
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use theme_settings::ThemeSettings;
use ui::{
    ActiveTheme, Button, Checkbox, Clickable, Color, Icon, IconName, Label, LabelCommon,
    ToggleState, h_flex, utils::WithRemSize, v_flex,
};
use workspace::item::{Item, ItemBufferKind, ItemEvent, ProjectItem, SaveOptions};
use zed_actions::{DecreaseBufferFontSize, IncreaseBufferFontSize, ResetBufferFontSize};

actions!(questionnaire_view, [OpenAsText]);

#[derive(Debug, Default, PartialEq)]
pub struct QuestionnaireDocument {
    pub sections: Vec<Section>,
}

#[derive(Debug, PartialEq)]
pub enum Section {
    Markdown(MarkdownChunk),
    Question(QuestionBlock),
}

#[derive(Debug, PartialEq)]
pub struct MarkdownChunk {
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub struct QuestionBlock {
    pub heading_row: u32,
    pub title: String,
    pub kind: QuestionKind,
    pub options: Vec<ChoiceOption>,
    pub answer: Option<AnswerLine>,
    pub read_only: Option<ReadOnlyReason>,
    pub fingerprint: Option<Fingerprint>,
    pub body: String,
    pub answer_tail: String,
}

#[derive(Debug, PartialEq)]
pub struct Fingerprint {
    pub row: u32,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    Single,
    Multi,
    SummaryConfirmation,
    Feedback,
    AssumptionConfirmation,
    PlanApproval,
}

#[derive(Debug, PartialEq)]
pub struct ChoiceOption {
    pub row: u32,
    pub letter: Option<char>,
    pub text: String,
    pub is_other: bool,
}

#[derive(Debug, PartialEq)]
pub struct AnswerLine {
    pub row: u32,
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyReason {
    DuplicateAnswer,
    MultiLineAnswer,
}

// Real questionnaires nest `### Q1.` under `## Topic`; level is not a signal.
static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}#{1,6}(?:[ \t]+|$)(.*)$").unwrap());
static QUESTION_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^Q(\d+)\b").unwrap());
static NUMBERED_HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:q(?:uestion)?[ \t]*)?\d+[ \t]*[.:)-]?[ \t]*$").unwrap());
static QUESTION_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:q(?:uestion)?[ \t]*)?\d+[ \t]*[:.)-][ \t]*").unwrap());
static LETTERED_OPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z])[.)][ \t]+(.+)$").unwrap());
static BULLET_OPTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[ \t]*[-*+][ \t]+(.+)$").unwrap());
static ANSWER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[Answer\]:[ \t]*(.*)$").unwrap());
static FINGERPRINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[Approval Fingerprint\]:[ \t]*(sha256:[0-9a-f]{64})?[ \t]*$").unwrap()
});
static INLINE_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<!--.*?-->").unwrap());
static FENCE_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}(`{3,}|~{3,})(.*)$").unwrap());

pub fn parse_questionnaire(text: &str) -> QuestionnaireDocument {
    let lines: Vec<String> = text
        .lines()
        .map(|line| INLINE_COMMENT.replace_all(line, "").into_owned())
        .collect();
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let inactive = inactive_rows(&lines);
    let heading_rows: Vec<usize> = (0..lines.len())
        .filter(|&row| !inactive[row] && heading_title(lines[row]).is_some())
        .collect();

    let mut sections = Vec::new();
    let preamble_end = heading_rows.first().copied().unwrap_or(lines.len());
    push_markdown(&mut sections, &lines[..preamble_end]);

    for (index, &heading_row) in heading_rows.iter().enumerate() {
        let end = heading_rows.get(index + 1).copied().unwrap_or(lines.len());
        let heading = heading_title(lines[heading_row]).unwrap_or_default();
        let body = &lines[heading_row + 1..end];
        let body_inactive = &inactive[heading_row + 1..end];
        match classify_heading(&heading, body, body_inactive) {
            Some(classified) => sections.push(Section::Question(parse_question(
                heading_row,
                classified,
                body,
                body_inactive,
            ))),
            None => push_markdown(&mut sections, &lines[heading_row..end]),
        }
    }
    QuestionnaireDocument { sections }
}

// Fenced code and multi-line comments hide headings, options and answers.
fn inactive_rows(lines: &[&str]) -> Vec<bool> {
    let mut inactive = vec![false; lines.len()];
    let mut fence: Option<(char, usize)> = None;
    let mut in_comment = false;

    for (row, line) in lines.iter().enumerate() {
        if let Some((marker, length)) = fence {
            inactive[row] = true;
            let trimmed = line.trim_start();
            let run = trimmed.chars().take_while(|&c| c == marker).count();
            if run >= length && trimmed[run..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if in_comment {
            inactive[row] = true;
            if line.contains("-->") {
                in_comment = false;
            }
            continue;
        }
        if let Some(captures) = FENCE_OPEN.captures(line) {
            let run = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
            let info = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
            if let Some(marker) = run.chars().next()
                && (marker == '~' || !info.contains('`'))
            {
                fence = Some((marker, run.len()));
                inactive[row] = true;
                continue;
            }
        }
        if line.contains("<!--") {
            in_comment = true;
            inactive[row] = true;
        }
    }
    inactive
}

struct ClassifiedHeading {
    title: String,
    kind: QuestionKind,
    label_line: Option<usize>,
}

fn classify_heading(heading: &str, body: &[&str], inactive: &[bool]) -> Option<ClassifiedHeading> {
    let numbered_only = NUMBERED_HEADING.is_match(heading);
    let label_line = if numbered_only {
        body.iter()
            .zip(inactive)
            .position(|(line, &inactive)| !inactive && !line.trim().is_empty())
    } else {
        None
    };
    let question_text = match label_line {
        Some(index) => strip_emphasis(body[index].trim()).to_string(),
        None => QUESTION_PREFIX.replace(heading, "").trim().to_string(),
    };
    let title = match label_line {
        Some(_) => format!("{heading} {question_text}"),
        None => heading.to_string(),
    };

    let label = strip_emphasis(question_text.trim_end_matches(['?', ':']).trim()).to_lowercase();
    let special = match label.as_str() {
        "consolidated summary confirmation" => Some(QuestionKind::SummaryConfirmation),
        "requested changes feedback" => Some(QuestionKind::Feedback),
        "assumption confirmation" => Some(QuestionKind::AssumptionConfirmation),
        "plan approval" => Some(QuestionKind::PlanApproval),
        _ => None,
    };
    let is_numbered =
        numbered_only || QUESTION_ID.is_match(heading) || QUESTION_PREFIX.is_match(heading);
    let kind = match special {
        Some(kind) => kind,
        None if !is_numbered => return None,
        None if question_text
            .to_lowercase()
            .contains("(select all that apply)") =>
        {
            QuestionKind::Multi
        }
        None => QuestionKind::Single,
    };
    Some(ClassifiedHeading {
        title,
        kind,
        label_line,
    })
}

fn strip_emphasis(text: &str) -> &str {
    text.trim_matches(['*', '_']).trim()
}

fn strip_quotes(text: &str) -> &str {
    text.trim_matches(['"', '\'']).trim()
}

fn heading_title(line: &str) -> Option<String> {
    let captures = HEADING.captures(line)?;
    let title = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
    let title = title.trim_end();
    let title = title
        .rsplit_once(char::is_whitespace)
        .filter(|(_, tail)| !tail.is_empty() && tail.chars().all(|c| c == '#'))
        .map(|(head, _)| head)
        .unwrap_or(title);
    Some(title.trim().to_string())
}

fn push_markdown(sections: &mut Vec<Section>, lines: &[&str]) {
    let text = lines.join("\n").trim().to_string();
    if text.is_empty() {
        return;
    }
    match sections.last_mut() {
        Some(Section::Markdown(previous)) => {
            previous.text.push_str("\n\n");
            previous.text.push_str(&text);
        }
        _ => sections.push(Section::Markdown(MarkdownChunk { text })),
    }
}

fn parse_question(
    heading_row: usize,
    heading: ClassifiedHeading,
    lines: &[&str],
    inactive: &[bool],
) -> QuestionBlock {
    let ClassifiedHeading {
        title,
        kind,
        label_line,
    } = heading;
    let lettered_options = kind != QuestionKind::Feedback;
    let bullet_options = matches!(
        kind,
        QuestionKind::SummaryConfirmation | QuestionKind::PlanApproval
    );

    let mut options = Vec::new();
    let mut answer = None;
    let mut read_only = None;
    let mut fingerprint = None;
    let mut body_lines: Vec<&str> = Vec::new();
    let mut tail_lines: Vec<&str> = Vec::new();

    for (offset, &line) in lines.iter().enumerate() {
        let row = (heading_row + 1 + offset) as u32;
        if label_line == Some(offset) {
            continue;
        }
        let inactive_line = inactive.get(offset).copied().unwrap_or(false);
        if !inactive_line && let Some(captures) = ANSWER.captures(line) {
            let raw = captures
                .get(1)
                .map(|m| m.as_str())
                .unwrap_or_default()
                .trim();
            if answer.is_none() {
                answer = Some(AnswerLine {
                    row,
                    raw: raw.to_string(),
                });
            } else {
                read_only = Some(ReadOnlyReason::DuplicateAnswer);
            }
            continue;
        }
        if answer.is_some() {
            if !line.trim().is_empty() {
                tail_lines.push(line);
                read_only.get_or_insert(ReadOnlyReason::MultiLineAnswer);
            }
            continue;
        }
        if inactive_line {
            body_lines.push(line);
            continue;
        }
        if kind == QuestionKind::PlanApproval
            && let Some(captures) = FINGERPRINT.captures(line)
        {
            fingerprint = Some(Fingerprint {
                row,
                value: captures.get(1).map(|m| m.as_str().to_string()),
            });
            continue;
        }
        if lettered_options && let Some(captures) = LETTERED_OPTION.captures(line) {
            let letter = captures.get(1).and_then(|m| m.as_str().chars().next());
            let text = strip_quotes(captures.get(2).map(|m| m.as_str()).unwrap_or_default());
            options.push(ChoiceOption {
                row,
                letter,
                text: text.to_string(),
                is_other: letter == Some('X') || text.starts_with("Other (please specify)"),
            });
            continue;
        }
        if bullet_options && let Some(captures) = BULLET_OPTION.captures(line) {
            let text = strip_quotes(captures.get(1).map(|m| m.as_str()).unwrap_or_default());
            options.push(ChoiceOption {
                row,
                letter: None,
                text: text.to_string(),
                is_other: text.starts_with("Other (please specify)"),
            });
            continue;
        }
        body_lines.push(line);
    }

    QuestionBlock {
        heading_row: heading_row as u32,
        title,
        kind,
        options,
        answer,
        read_only,
        fingerprint,
        body: body_lines.join("\n").trim().to_string(),
        answer_tail: tail_lines.join("\n").trim().to_string(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub choices: Vec<usize>,
    pub other: Option<String>,
}

impl Answer {
    pub fn is_empty(&self) -> bool {
        self.choices.is_empty() && self.other.is_none()
    }
}

static LETTER_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z])(?:[.):]|\b)").unwrap());

// Ordinary questions store letters (`A`, `A, C, X: text`); special
// sections keep the literal formats the AI-DLC guards match on.
fn uses_letter_tokens(block: &QuestionBlock) -> bool {
    matches!(block.kind, QuestionKind::Single | QuestionKind::Multi)
        && block.options.iter().any(|option| option.letter.is_some())
}

fn other_option(block: &QuestionBlock) -> Option<(usize, char)> {
    block
        .options
        .iter()
        .enumerate()
        .find(|(_, option)| option.is_other)
        .map(|(index, option)| (index, option.letter.unwrap_or('X')))
}

pub fn parse_answer(block: &QuestionBlock) -> Answer {
    let raw = block
        .answer
        .as_ref()
        .map(|answer| answer.raw.trim())
        .unwrap_or_default();
    if raw.is_empty() || raw.chars().all(|c| c == '_') {
        return Answer::default();
    }
    let parsed = if uses_letter_tokens(block) {
        parse_letter_tokens(block, raw)
    } else if block.kind == QuestionKind::Feedback {
        None
    } else {
        option_index(block, raw).map(|index| Answer {
            choices: vec![index],
            other: None,
        })
    };
    parsed.unwrap_or_else(|| Answer {
        choices: Vec::new(),
        other: Some(raw.to_string()),
    })
}

fn parse_letter_tokens(block: &QuestionBlock, raw: &str) -> Option<Answer> {
    let (letters_part, other) = match other_option(block).and_then(|(index, letter)| {
        let pattern = format!(r"(?:^|,)[ \t]*{letter}(?:[.:]|[ \t]|$)[ \t]*(.*)$");
        let captures = Regex::new(&pattern).ok()?.captures(raw)?;
        let start = captures.get(0)?.start();
        let text = captures
            .get(1)
            .map(|m| m.as_str().trim())
            .unwrap_or_default();
        let placeholder = block.options.get(index).map(|option| option.text.as_str());
        let text = if Some(text) == placeholder { "" } else { text };
        Some((start, text.to_string()))
    }) {
        Some((start, text)) => (&raw[..start], Some(text)),
        None => (raw, None),
    };

    let mut choices = Vec::new();
    for token in letters_part.split(',').map(str::trim) {
        if token.is_empty() {
            continue;
        }
        let letter = LETTER_PREFIX
            .captures(token)?
            .get(1)?
            .as_str()
            .chars()
            .next()?;
        let index = block
            .options
            .iter()
            .position(|option| option.letter == Some(letter) && !option.is_other)?;
        choices.push(index);
    }
    choices.sort_unstable();
    choices.dedup();
    if choices.is_empty() && other.is_none() {
        return None;
    }
    Some(Answer { choices, other })
}

fn option_index(block: &QuestionBlock, token: &str) -> Option<usize> {
    let letter = LETTER_PREFIX
        .captures(token)
        .and_then(|captures| captures.get(1))
        .and_then(|m| m.as_str().chars().next());
    if let Some(letter) = letter
        && let Some(index) = block
            .options
            .iter()
            .position(|option| option.letter == Some(letter))
    {
        return Some(index);
    }
    let text = strip_quotes(token).to_lowercase();
    block
        .options
        .iter()
        .position(|option| option.text.to_lowercase() == text)
}

pub fn render_answer(block: &QuestionBlock, answer: &Answer) -> String {
    if answer.is_empty() {
        return String::new();
    }
    if uses_letter_tokens(block) {
        let mut letters: Vec<char> = answer
            .choices
            .iter()
            .filter_map(|&index| block.options.get(index))
            .filter_map(|option| option.letter)
            .collect();
        letters.sort_unstable();
        let mut tokens: Vec<String> = letters.iter().map(char::to_string).collect();
        if let Some(text) = &answer.other {
            tokens.push(match (other_option(block), text.is_empty()) {
                (Some((_, letter)), true) => letter.to_string(),
                (Some((_, letter)), false) => format!("{letter}: {text}"),
                (None, _) => text.clone(),
            });
        }
        return tokens.join(", ");
    }
    match answer
        .choices
        .first()
        .and_then(|&index| block.options.get(index))
    {
        Some(option) => match option.letter {
            Some(letter) => format!("{letter}. {}", option.text),
            None => option.text.clone(),
        },
        None => answer.other.clone().unwrap_or_default().trim().to_string(),
    }
}

pub fn answer_line(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        "[Answer]:".to_string()
    } else {
        format!("[Answer]: {raw}")
    }
}

pub fn toggle_option(block: &QuestionBlock, current: &Answer, index: usize) -> Answer {
    if block.kind == QuestionKind::Multi {
        let mut choices = current.choices.clone();
        match choices.iter().position(|&selected| selected == index) {
            Some(position) => {
                choices.remove(position);
            }
            None => choices.push(index),
        }
        choices.sort_unstable();
        Answer {
            choices,
            other: current.other.clone(),
        }
    } else if current.choices.as_slice() == [index] {
        Answer::default()
    } else {
        Answer {
            choices: vec![index],
            other: None,
        }
    }
}

pub fn with_other(block: &QuestionBlock, current: &Answer, text: &str) -> Answer {
    let text = text.trim();
    let other = (!text.is_empty()).then(|| text.to_string());
    let choices = if block.kind == QuestionKind::Multi {
        current.choices.clone()
    } else {
        Vec::new()
    };
    Answer { choices, other }
}

pub struct QuestionnaireItem {
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    buffer: Entity<Buffer>,
    dirty: bool,
    _subscription: Subscription,
}

impl QuestionnaireItem {
    pub fn buffer(&self) -> Entity<Buffer> {
        self.buffer.clone()
    }
}

impl project::ProjectItem for QuestionnaireItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        let is_questionnaire = path
            .path
            .file_name()
            .is_some_and(|name| name.ends_with("-questions.md"));
        if !is_questionnaire {
            return None;
        }
        let path = path.clone();
        let project = project.clone();
        Some(cx.spawn(async move |cx| {
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                .await?;
            let entry_id = project.read_with(cx, |project, cx| {
                project.entry_for_path(&path, cx).map(|entry| entry.id)
            });
            Ok(cx.new(|cx| {
                // `is_dirty` has no `cx`, so mirror the buffer's flag here.
                let subscription =
                    cx.subscribe(&buffer, |this: &mut Self, buffer, _: &BufferEvent, cx| {
                        this.dirty = buffer.read(cx).is_dirty();
                    });
                Self {
                    project_path: path,
                    entry_id,
                    dirty: buffer.read(cx).is_dirty(),
                    buffer,
                    _subscription: subscription,
                }
            }))
        }))
    }

    fn entry_id(&self, _cx: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _cx: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }
}

pub enum QuestionnaireEvent {
    Edited,
}

struct EditingTarget {
    kind: QuestionKind,
    ordinal: usize,
    title: String,
}

pub struct QuestionnaireView {
    item: Entity<QuestionnaireItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    document: QuestionnaireDocument,
    section_markdown: Vec<Option<Entity<Markdown>>>,
    tail_markdown: Vec<Option<Entity<Markdown>>>,
    text_input: Entity<Editor>,
    editing: Option<EditingTarget>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<QuestionnaireEvent> for QuestionnaireView {}

impl Focusable for QuestionnaireView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl QuestionnaireView {
    pub fn buffer(&self, cx: &App) -> Entity<Buffer> {
        self.item.read(cx).buffer()
    }

    fn reparse(&mut self, cx: &mut Context<Self>) {
        let text = self.buffer(cx).read(cx).text();
        self.document = parse_questionnaire(&text);
        let language_registry = self.project.read(cx).languages().clone();
        let count = self.document.sections.len();
        self.section_markdown.resize_with(count, || None);
        self.tail_markdown.resize_with(count, || None);
        for (index, section) in self.document.sections.iter().enumerate() {
            let (main, tail) = match section {
                Section::Markdown(chunk) => (chunk.text.as_str(), ""),
                Section::Question(block) => (block.body.as_str(), block.answer_tail.as_str()),
            };
            sync_markdown(
                &mut self.section_markdown[index],
                main,
                &language_registry,
                cx,
            );
            sync_markdown(&mut self.tail_markdown[index], tail, &language_registry, cx);
        }
        cx.emit(QuestionnaireEvent::Edited);
        cx.notify();
    }

    fn question(&self, section_index: usize) -> Option<&QuestionBlock> {
        match self.document.sections.get(section_index)? {
            Section::Question(block) => Some(block),
            Section::Markdown(_) => None,
        }
    }

    fn progress(&self) -> (usize, usize) {
        let mut answered = 0;
        let mut total = 0;
        for section in &self.document.sections {
            if let Section::Question(block) = section {
                total += 1;
                if !parse_answer(block).is_empty() {
                    answered += 1;
                }
            }
        }
        (answered, total)
    }

    fn is_editing(&self, section_index: usize) -> bool {
        self.editing.as_ref().and_then(|target| self.locate(target)) == Some(section_index)
    }

    fn ordinal_of(&self, section_index: usize) -> usize {
        let Some(block) = self.question(section_index) else {
            return 0;
        };
        self.document.sections[..section_index]
            .iter()
            .filter(
                |section| matches!(section, Section::Question(other) if other.kind == block.kind),
            )
            .count()
    }

    fn locate(&self, target: &EditingTarget) -> Option<usize> {
        self.document
            .sections
            .iter()
            .enumerate()
            .filter(|(_, section)| matches!(section, Section::Question(block) if block.kind == target.kind))
            .nth(target.ordinal)
            .filter(|(_, section)| matches!(section, Section::Question(block) if block.title == target.title))
            .map(|(index, _)| index)
    }

    fn on_option_clicked(
        &mut self,
        section_index: usize,
        option_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_editing(window, cx);
        let Some(block) = self.question(section_index) else {
            return;
        };
        if block.answer.is_none() || block.read_only.is_some() {
            return;
        }
        let Some(option) = block.options.get(option_index) else {
            return;
        };
        let answer = parse_answer(block);
        if option.is_other {
            if answer.other.is_some() {
                let raw = render_answer(block, &with_other(block, &answer, ""));
                self.write_answer(section_index, &raw, cx);
            } else {
                self.start_editing(section_index, window, cx);
            }
            return;
        }
        let next = toggle_option(block, &answer, option_index);
        let raw = render_answer(block, &next);
        self.write_answer(section_index, &raw, cx);
    }

    fn start_editing(&mut self, section_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editing(window, cx);
        let Some(block) = self.question(section_index) else {
            return;
        };
        let initial = parse_answer(block).other.unwrap_or_default();
        self.editing = Some(EditingTarget {
            kind: block.kind,
            ordinal: self.ordinal_of(section_index),
            title: block.title.clone(),
        });
        self.text_input.update(cx, |editor, cx| {
            editor.set_text(initial, window, cx);
        });
        window.focus(&self.text_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn commit_editing(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.editing.take() else {
            return;
        };
        cx.notify();
        let Some(section_index) = self.locate(&target) else {
            return;
        };
        let text = self.text_input.read(cx).text(cx);
        let Some(block) = self.question(section_index) else {
            return;
        };
        let raw = render_answer(block, &with_other(block, &parse_answer(block), &text));
        self.write_answer(section_index, &raw, cx);
    }

    fn cancel_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn write_answer(&mut self, section_index: usize, raw: &str, cx: &mut Context<Self>) {
        let Some(block) = self.question(section_index) else {
            return;
        };
        let Some(answer) = &block.answer else {
            return;
        };
        let row = answer.row;
        let line = answer_line(raw);
        let buffer = self.buffer(cx);
        buffer.update(cx, |buffer, cx| {
            let line_len = buffer.line_len(row);
            buffer.edit(
                [(Point::new(row, 0)..Point::new(row, line_len), line)],
                None,
                cx,
            );
        });
        self.project
            .update(cx, |project, cx| project.save_buffer(buffer, cx))
            .detach_and_log_err(cx);
    }

    // Shares the markdown preview's font size so cmd-+/cmd-- feel the same.
    fn adjust_font_size(&mut self, persist: bool, delta: Pixels, cx: &mut Context<Self>) {
        if persist {
            let fs = self.project.read(cx).fs().clone();
            update_settings_file(fs, cx, move |settings, cx| {
                let size = ThemeSettings::get_global(cx).markdown_preview_font_size(cx) + delta;
                settings.theme.markdown_preview_font_size =
                    Some(f32::from(theme_settings::clamp_font_size(size)).into());
            });
        } else {
            theme_settings::adjust_markdown_preview_font_size(cx, |size| size + delta);
        }
    }

    fn reset_font_size(&mut self, persist: bool, cx: &mut Context<Self>) {
        if persist {
            let fs = self.project.read(cx).fs().clone();
            update_settings_file(fs, cx, move |settings, _| {
                settings.theme.markdown_preview_font_size = None;
            });
        } else {
            theme_settings::reset_markdown_preview_font_size(cx);
        }
    }

    fn render_section(
        &self,
        index: usize,
        section: &Section,
        style: &MarkdownStyle,
        cx: &Context<Self>,
    ) -> AnyElement {
        match section {
            Section::Markdown(_) => match &self.section_markdown[index] {
                Some(markdown) => {
                    MarkdownElement::new(markdown.clone(), style.clone()).into_any_element()
                }
                None => div().into_any_element(),
            },
            Section::Question(block) => self.render_question(index, block, style, cx),
        }
    }

    fn render_question(
        &self,
        index: usize,
        block: &QuestionBlock,
        style: &MarkdownStyle,
        cx: &Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let small = rems(0.85);
        let answer = parse_answer(block);
        let disabled = block.answer.is_none() || block.read_only.is_some();
        let raw = block
            .answer
            .as_ref()
            .map(|answer| answer.raw.clone())
            .unwrap_or_default();
        let notice = match (&block.answer, block.read_only) {
            (None, _) => Some("No [Answer]: line. Edit as text."),
            (_, Some(ReadOnlyReason::DuplicateAnswer)) => {
                Some("Duplicate [Answer]: line. Edit as text.")
            }
            (_, Some(ReadOnlyReason::MultiLineAnswer)) => Some("Multi-line answer. Edit as text."),
            _ => None,
        };
        let show_free_text = block.kind == QuestionKind::Feedback
            || answer.other.is_some()
            || self.is_editing(index);

        v_flex()
            .gap_2()
            .p_3()
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(rems(1.1))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(block.title.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(small)
                            .text_color(if answer.is_empty() {
                                colors.text_muted
                            } else {
                                status.success
                            })
                            .child(if raw.is_empty() {
                                "unanswered".to_string()
                            } else {
                                raw
                            }),
                    ),
            )
            .when_some(self.section_markdown[index].clone(), |this, markdown| {
                this.child(MarkdownElement::new(markdown, style.clone()))
            })
            .when_some(block.fingerprint.as_ref(), |this, fingerprint| {
                let value = fingerprint.value.as_deref().unwrap_or("not computed");
                this.child(
                    div()
                        .text_size(small)
                        .text_color(colors.text_muted)
                        .child(format!("Fingerprint: {value}")),
                )
            })
            .children(
                block
                    .options
                    .iter()
                    .enumerate()
                    .map(|(option_index, option)| {
                        let checked = answer.choices.contains(&option_index)
                            || (option.is_other && answer.other.is_some());
                        let state = if checked {
                            ToggleState::Selected
                        } else {
                            ToggleState::Unselected
                        };
                        let label = match option.letter {
                            Some(letter) => format!("{letter}. {}", option.text),
                            None => option.text.clone(),
                        };
                        h_flex()
                            .id((SharedString::from(format!("row{index}")), option_index))
                            .gap_2()
                            .items_start()
                            .when(!disabled, |this| {
                                this.cursor_pointer().on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.on_option_clicked(index, option_index, window, cx);
                                    },
                                ))
                            })
                            .child(
                                Checkbox::new(
                                    (SharedString::from(format!("q{index}")), option_index),
                                    state,
                                )
                                .disabled(disabled)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.on_option_clicked(index, option_index, window, cx);
                                    },
                                )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(rems(1.0))
                                    .when(checked, |this| this.font_weight(FontWeight::BOLD))
                                    .child(label),
                            )
                    }),
            )
            .when(show_free_text && !disabled, |this| {
                if self.is_editing(index) {
                    this.child(self.text_input.clone())
                } else {
                    let text = answer
                        .other
                        .clone()
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| "Click to type an answer".to_string());
                    this.child(
                        div()
                            .id(("free-text", index))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.start_editing(index, window, cx);
                            }))
                            .text_size(rems(1.0))
                            .text_color(colors.text_muted)
                            .child(text),
                    )
                }
            })
            .when_some(self.tail_markdown[index].clone(), |this, markdown| {
                this.child(MarkdownElement::new(markdown, style.clone()))
            })
            .when_some(notice, |this, notice| {
                this.child(
                    div()
                        .text_size(small)
                        .text_color(status.warning)
                        .child(notice),
                )
            })
            .into_any_element()
    }
}

fn sync_markdown(
    slot: &mut Option<Entity<Markdown>>,
    text: &str,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut Context<QuestionnaireView>,
) {
    if text.is_empty() {
        *slot = None;
        return;
    }
    let source: SharedString = text.to_string().into();
    match slot {
        Some(markdown) => markdown.update(cx, |markdown, cx| markdown.reset(source, cx)),
        None => {
            *slot =
                Some(cx.new(|cx| Markdown::new(source, Some(language_registry.clone()), None, cx)))
        }
    }
}

impl Render for QuestionnaireView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let style = MarkdownStyle::themed(MarkdownFont::Preview, window, cx);
        let font_size = ThemeSettings::get_global(cx).markdown_preview_font_size(cx);
        let (answered, total) = self.progress();
        let sections: Vec<AnyElement> = self
            .document
            .sections
            .iter()
            .enumerate()
            .map(|(index, section)| self.render_section(index, section, &style, cx))
            .collect();

        v_flex()
            .id("questionnaire-view")
            .key_context("QuestionnaireView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.commit_editing(window, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                this.cancel_editing(window, cx);
            }))
            .on_action(cx.listener(|this, action: &IncreaseBufferFontSize, _, cx| {
                this.adjust_font_size(action.persist, px(1.0), cx);
            }))
            .on_action(cx.listener(|this, action: &DecreaseBufferFontSize, _, cx| {
                this.adjust_font_size(action.persist, px(-1.0), cx);
            }))
            .on_action(cx.listener(|this, action: &ResetBufferFontSize, _, cx| {
                this.reset_font_size(action.persist, cx);
            }))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new(format!("{answered}/{total} answered")).color(Color::Muted))
                    .child(Button::new("open-as-text", "Open as text").on_click(
                        |_, window, cx| {
                            window.dispatch_action(OpenAsText.boxed_clone(), cx);
                        },
                    )),
            )
            .child(
                // Everything below scales with the rem size, like the preview.
                WithRemSize::new(font_size).flex_1().min_h_0().child(
                    v_flex()
                        .id("questionnaire-body")
                        .size_full()
                        .overflow_y_scroll()
                        .p_4()
                        .gap_4()
                        .children(sections),
                ),
            )
    }
}

impl Item for QuestionnaireView {
    type Event = QuestionnaireEvent;

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.item
            .read(cx)
            .project_path
            .path
            .file_name()
            .unwrap_or("questions")
            .to_string()
            .into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileGeneric).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(
            self.item
                .read(cx)
                .project_path
                .path
                .as_unix_str()
                .to_string()
                .into(),
        )
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("agentium questionnaire")
    }

    fn to_item_events(event: &QuestionnaireEvent, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            QuestionnaireEvent::Edited => {
                f(ItemEvent::Edit);
                f(ItemEvent::UpdateTab);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        f(self.item.entity_id(), self.item.read(cx))
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer(cx).read(cx).is_dirty()
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.buffer(cx).read(cx).has_conflict()
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    // Formatting is skipped: it could reflow lines the AI-DLC tools parse.
    fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer(cx);
        project.update(cx, |project, cx| project.save_buffer(buffer, cx))
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer(cx);
        let reload = project.update(cx, |project, cx| {
            project.reload_buffers(HashSet::from_iter([buffer]), true, cx)
        });
        cx.spawn(async move |_this, _cx| {
            reload.await?;
            Ok(())
        })
    }
}

impl ProjectItem for QuestionnaireView {
    type Item = QuestionnaireItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&workspace::Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = item.read(cx).buffer();
        let text_input = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Type your answer", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.subscribe(&buffer, |this, _, event: &BufferEvent, cx| {
                if matches!(
                    event,
                    BufferEvent::Edited { .. }
                        | BufferEvent::Reloaded
                        | BufferEvent::DirtyChanged
                        | BufferEvent::Saved
                        | BufferEvent::FileHandleChanged
                ) {
                    this.reparse(cx);
                }
            }),
            cx.subscribe_in(
                &text_input,
                window,
                |this, _, event: &EditorEvent, window, cx| {
                    if let EditorEvent::Blurred = event {
                        this.commit_editing(window, cx);
                    }
                },
            ),
        ];
        let mut view = Self {
            item,
            project,
            focus_handle: cx.focus_handle(),
            document: QuestionnaireDocument::default(),
            section_markdown: Vec::new(),
            tail_markdown: Vec::new(),
            text_input,
            editing: None,
            _subscriptions: subscriptions,
        };
        view.reparse(cx);
        view
    }
}

#[cfg(test)]
mod tests {
    // No `use super::*`: it would pull in `gpui::test` and shadow `#[test]`.
    use super::{
        Answer, AnswerLine, ChoiceOption, MarkdownChunk, QuestionKind, Section, answer_line,
        parse_answer, parse_questionnaire, render_answer, toggle_option, with_other,
    };
    use regex::Regex;

    fn question(section: &Section) -> &super::QuestionBlock {
        match section {
            Section::Question(block) => block,
            other => panic!("expected question, got {other:?}"),
        }
    }

    #[test]
    fn empty_text_has_no_sections() {
        assert!(parse_questionnaire("").sections.is_empty());
    }

    #[test]
    fn plain_markdown_is_one_chunk() {
        let document = parse_questionnaire("# Title\n\nsome text\n");
        assert_eq!(
            document.sections,
            vec![Section::Markdown(MarkdownChunk {
                text: "# Title\n\nsome text".to_string(),
            })]
        );
    }

    #[test]
    fn q_heading_with_options_and_blank_answer() {
        let document =
            parse_questionnaire("## Q1. What?\n\nA. Foo\nX. Other (please specify)\n\n[Answer]:\n");
        assert_eq!(document.sections.len(), 1);
        let block = question(&document.sections[0]);
        assert_eq!(block.heading_row, 0);
        assert_eq!(block.title, "Q1. What?");
        assert_eq!(block.kind, QuestionKind::Single);
        assert_eq!(
            block.options,
            vec![
                ChoiceOption {
                    row: 2,
                    letter: Some('A'),
                    text: "Foo".to_string(),
                    is_other: false,
                },
                ChoiceOption {
                    row: 3,
                    letter: Some('X'),
                    text: "Other (please specify)".to_string(),
                    is_other: true,
                },
            ]
        );
        assert_eq!(
            block.answer,
            Some(AnswerLine {
                row: 5,
                raw: String::new(),
            })
        );
        assert_eq!(block.read_only, None);
        assert_eq!(block.body, "");
    }

    #[test]
    fn h3_questions_under_h2_topics() {
        let document = parse_questionnaire(
            "# Title\n\n## Way of Working\n\n### Q1. Merge?\n\nHow?\n\nA. a\nB. b\nX. Other (please specify)\n\n[Answer]:\n\n### Q2. Next?\n\nA. a\n\n[Answer]: A. a\n\n## Other topic\n\ntext\n",
        );
        assert_eq!(document.sections.len(), 4);
        assert_eq!(
            document.sections[0],
            Section::Markdown(MarkdownChunk {
                text: "# Title\n\n## Way of Working".to_string(),
            })
        );
        let first = question(&document.sections[1]);
        assert_eq!(first.title, "Q1. Merge?");
        assert_eq!(first.body, "How?");
        assert_eq!(first.options.len(), 3);
        assert_eq!(question(&document.sections[2]).title, "Q2. Next?");
        assert_eq!(
            document.sections[3],
            Section::Markdown(MarkdownChunk {
                text: "## Other topic\n\ntext".to_string(),
            })
        );
    }

    #[test]
    fn heading_level_is_not_a_signal() {
        let document = parse_questionnaire(
            "### Q1. A?\n\nA. a\n\n[Answer]:\n\n#### Q2. B?\n\nA. a\n\n[Answer]:\n\n# Q3. C?\n\nA. a\n\n[Answer]:\n",
        );
        assert_eq!(question_count(&document), 3);
        assert_eq!(question(&document.sections[2]).title, "Q3. C?");
    }

    #[test]
    fn non_question_h2_is_markdown() {
        let document = parse_questionnaire(
            "## Sources\n\n- [desc] x\n\n## Q1. A?\n\nA. a\n\n[Answer]: A. a\n",
        );
        assert_eq!(document.sections.len(), 2);
        assert_eq!(
            document.sections[0],
            Section::Markdown(MarkdownChunk {
                text: "## Sources\n\n- [desc] x".to_string(),
            })
        );
        assert_eq!(question(&document.sections[1]).heading_row, 4);
    }

    const FIXTURE: &str = "# Intent Capture Questions\n\n## Sources\n\n- [desc] Initial description: \"Build a local CLI that echoes supplied text.\"\n- [scope] Workflow-selected scope: `poc`.\n- [memory:M1] `aidlc/spaces/default/memory/project.md#Forbidden`: \"Do not add network access.\"\n\n## Q1. What business problem are we solving?\n\nA. Echo supplied text locally.\nX. Other (please specify)\n\n[Answer]: A. Echo supplied text locally.\n\n## Q2. Who is the customer?\n\nA. The requester.\nX. Other (please specify)\n\n[Answer]: A. The requester.\n\n## Q3. What does success look like?\n\nA. The output exactly matches the supplied text.\nX. Other (please specify)\n\n[Answer]: A. The output exactly matches the supplied text.\n\n## Q4. What triggered the initiative?\n\nA. A small workflow exercise.\nX. Other (please specify)\n\n[Answer]: A. A small workflow exercise.\n\n## Q5. Who are the key stakeholders?\n\nA. The requester only.\nB. Not identified.\nX. Other (please specify)\n\n[Answer]: A. The requester only.\n\n## Q6. Who decides and who influences?\n\nA. The requester decides; no other influencers are identified.\nX. Other (please specify)\n\n[Answer]: A. The requester decides; no other influencers are identified.\n\n## Q7. What communication is required?\n\nA. None.\nX. Other (please specify)\n\n[Answer]: A. None.\n\n## Q8. Does the workflow-selected scope match the product boundary?\n\nA. Yes, keep the product boundary to a proof of concept.\nX. Other (please specify)\n\n[Answer]: A. Yes, keep the product boundary to a proof of concept.\n";

    #[test]
    fn fixture_has_eight_answered_questions() {
        let document = parse_questionnaire(FIXTURE);
        let questions: Vec<_> = document
            .sections
            .iter()
            .filter_map(|section| match section {
                Section::Question(block) => Some(block),
                Section::Markdown(_) => None,
            })
            .collect();
        assert_eq!(questions.len(), 8);
        assert_eq!(
            document.sections.len(),
            9,
            "title and Sources merge into one chunk"
        );
        for block in &questions {
            assert_eq!(block.kind, QuestionKind::Single);
            assert!(block.options.last().is_some_and(|option| option.is_other));
            assert!(
                block
                    .answer
                    .as_ref()
                    .is_some_and(|answer| answer.raw.starts_with("A. "))
            );
            assert_eq!(block.read_only, None);
        }
        assert_eq!(questions[4].options.len(), 3);
    }

    #[test]
    fn select_all_that_apply_is_multi() {
        let document = parse_questionnaire(
            "## Q2. Which? (select all that apply)\n\nA. a\nB. b\n\n[Answer]: A, B\n",
        );
        assert_eq!(question(&document.sections[0]).kind, QuestionKind::Multi);
    }

    #[test]
    fn bullets_in_ordinary_question_are_body() {
        let document =
            parse_questionnaire("## Q1. Why?\n\n- note one\n- note two\n\nA. a\n\n[Answer]:\n");
        let block = question(&document.sections[0]);
        assert_eq!(block.options.len(), 1);
        assert_eq!(block.body, "- note one\n- note two");
    }

    #[test]
    fn lines_after_answer_make_it_read_only() {
        let document =
            parse_questionnaire("## Q1. Why?\n\nA. a\n\n[Answer]: A. a\n**Mode:** chat\n");
        let block = question(&document.sections[0]);
        assert_eq!(
            block.read_only,
            Some(super::ReadOnlyReason::MultiLineAnswer)
        );
        assert_eq!(block.answer_tail, "**Mode:** chat");
        assert_eq!(block.body, "");
    }

    fn option_texts(block: &super::QuestionBlock) -> Vec<(Option<char>, &str)> {
        block
            .options
            .iter()
            .map(|option| (option.letter, option.text.as_str()))
            .collect()
    }

    #[test]
    fn consolidated_summary_confirmation_uses_bullets() {
        let document = parse_questionnaire(
            "## Consolidated Summary Confirmation\n\nDoes this all look correct?\n\n- Looks correct\n- Request changes\n\n[Answer]: Looks correct\n",
        );
        let block = question(&document.sections[0]);
        assert_eq!(block.kind, QuestionKind::SummaryConfirmation);
        assert_eq!(
            option_texts(block),
            vec![(None, "Looks correct"), (None, "Request changes")]
        );
        assert_eq!(block.body, "Does this all look correct?");
        assert_eq!(
            block.answer.as_ref().map(|answer| answer.raw.as_str()),
            Some("Looks correct")
        );
    }

    #[test]
    fn requested_changes_feedback_is_free_text() {
        let document = parse_questionnaire(
            "## Requested Changes Feedback\n\nWhat should change?\n\n[Answer]: Tighten Q3\n",
        );
        let block = question(&document.sections[0]);
        assert_eq!(block.kind, QuestionKind::Feedback);
        assert!(block.options.is_empty());
        assert_eq!(block.body, "What should change?");
    }

    #[test]
    fn assumption_confirmation_bullets_are_body() {
        let document = parse_questionnaire(
            "## Assumption Confirmation\n\n- [assumption] Local only.\n- [assumption] No auth.\n\nA. Accept assumptions\nB. Convert to follow-up questions\n\n[Answer]:\n",
        );
        let block = question(&document.sections[0]);
        assert_eq!(block.kind, QuestionKind::AssumptionConfirmation);
        assert_eq!(
            option_texts(block),
            vec![
                (Some('A'), "Accept assumptions"),
                (Some('B'), "Convert to follow-up questions"),
            ]
        );
        assert_eq!(
            block.body,
            "- [assumption] Local only.\n- [assumption] No auth."
        );
    }

    const SHA: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn plan_approval_with_fingerprint() {
        let text = format!(
            "## Plan Approval\n[Approval Fingerprint]: {SHA}\nA. Approve Plan\nB. Request Changes\n[Answer]: A. Approve Plan\n"
        );
        let document = parse_questionnaire(&text);
        let block = question(&document.sections[0]);
        assert_eq!(block.kind, QuestionKind::PlanApproval);
        assert_eq!(
            block.fingerprint,
            Some(super::Fingerprint {
                row: 1,
                value: Some(SHA.to_string()),
            })
        );
        assert_eq!(
            option_texts(block),
            vec![(Some('A'), "Approve Plan"), (Some('B'), "Request Changes")]
        );
        assert_eq!(block.body, "");
    }

    #[test]
    fn plan_approval_bullet_options_and_empty_fingerprint() {
        let document = parse_questionnaire(
            "## Plan Approval\n[Approval Fingerprint]:\n- \"Approve Plan\"\n- \"Request Changes\"\n[Answer]:\n",
        );
        let block = question(&document.sections[0]);
        assert_eq!(block.kind, QuestionKind::PlanApproval);
        assert_eq!(
            block.fingerprint,
            Some(super::Fingerprint {
                row: 1,
                value: None,
            })
        );
        assert_eq!(
            option_texts(block),
            vec![(None, "Approve Plan"), (None, "Request Changes")]
        );
    }

    #[test]
    fn plan_approval_heading_variants() {
        for text in [
            "## Q1: Plan Approval\n[Answer]: A. Approve Plan\n",
            "## Question 1 - Plan Approval\n[Answer]: A. Approve Plan\n",
            "## Q1\n\nPlan Approval\n\nA. Approve Plan\nB. Request Changes\n[Answer]: A. Approve Plan\n",
            "## Question 1\n\n**Plan Approval**\n[Answer]: A. Approve Plan\n",
            "## Plan <!-- heading -->Approval\n[Answer]: A. Approve <!-- answer -->Plan\n",
        ] {
            let document = parse_questionnaire(text);
            let block = question(&document.sections[0]);
            assert_eq!(block.kind, QuestionKind::PlanApproval, "{text:?}");
            assert_eq!(
                block.answer.as_ref().map(|answer| answer.raw.as_str()),
                Some("A. Approve Plan"),
                "{text:?}"
            );
            assert!(!block.body.contains("Plan Approval"), "{text:?}");
        }
    }

    #[test]
    fn numbered_heading_takes_question_text_from_body() {
        let document = parse_questionnaire(
            "## Q1\n\nWhich checkpoint applies?\n\nA. Plan Approval\n[Answer]: A. Approve Plan\n",
        );
        let block = question(&document.sections[0]);
        assert_eq!(block.kind, QuestionKind::Single);
        assert_eq!(block.title, "Q1 Which checkpoint applies?");
        assert_eq!(block.body, "");
        assert_eq!(option_texts(block), vec![(Some('A'), "Plan Approval")]);
    }

    fn question_count(document: &super::QuestionnaireDocument) -> usize {
        document
            .sections
            .iter()
            .filter(|section| matches!(section, Section::Question(_)))
            .count()
    }

    #[test]
    fn fenced_headings_are_inactive() {
        for text in [
            "```markdown\n## Plan Approval\n[Answer]: A. Approve Plan\n```\n",
            "~~~markdown\n## Q1\nPlan Approval\n[Answer]: A. Approve Plan\n~~~\n",
        ] {
            let document = parse_questionnaire(text);
            assert_eq!(question_count(&document), 0, "{text:?}");
            assert_eq!(document.sections.len(), 1, "{text:?}");
        }
    }

    #[test]
    fn multi_line_comment_is_inactive() {
        let document = parse_questionnaire(
            "<!--\n## Plan Approval\n[Answer]: A. Approve Plan\n-->\n## Plan Approval\n[Answer]:\n",
        );
        assert_eq!(question_count(&document), 1);
        let block = question(&document.sections[1]);
        assert_eq!(block.kind, QuestionKind::PlanApproval);
        assert_eq!(
            block.answer.as_ref().map(|answer| answer.raw.as_str()),
            Some("")
        );
    }

    #[test]
    fn longer_fence_is_not_closed_by_shorter() {
        let document = parse_questionnaire(
            "````\n```\n## Q1. Hidden?\n```\n````\n## Q2. Real?\n\nA. a\n\n[Answer]:\n",
        );
        assert_eq!(question_count(&document), 1);
        assert_eq!(question(&document.sections[1]).title, "Q2. Real?");
    }

    #[test]
    fn fence_inside_question_is_body() {
        let document = parse_questionnaire(
            "## Q1. How?\n\n```\nA. not an option\n[Answer]: not an answer\n```\n\nA. real\n\n[Answer]:\n",
        );
        let block = question(&document.sections[0]);
        assert_eq!(option_texts(block), vec![(Some('A'), "real")]);
        assert_eq!(
            block.answer.as_ref().map(|answer| answer.raw.as_str()),
            Some("")
        );
        assert_eq!(
            block.body,
            "```\nA. not an option\n[Answer]: not an answer\n```"
        );
        assert_eq!(block.read_only, None);
    }

    fn block_with_answer(kind_text: &str, raw: &str) -> super::QuestionBlock {
        let mut document = parse_questionnaire(&format!("{kind_text}[Answer]: {raw}\n"));
        match document.sections.pop() {
            Some(Section::Question(block)) => block,
            other => panic!("expected question, got {other:?}"),
        }
    }

    const SINGLE: &str =
        "## Q1. What?\n\nA. Echo text.\nB. Something else.\nX. Other (please specify)\n\n";
    const MULTI: &str =
        "## Q2. Which? (select all that apply)\n\nA. a\nB. b\nC. c\nX. Other (please specify)\n\n";
    const SUMMARY: &str =
        "## Consolidated Summary Confirmation\n\n- Looks correct\n- Request changes\n\n";
    const PLAN: &str = "## Plan Approval\nA. Approve Plan\nB. Request Changes\n";
    const FEEDBACK: &str = "## Requested Changes Feedback\n\n";

    fn choices(indices: &[usize]) -> Answer {
        Answer {
            choices: indices.to_vec(),
            other: None,
        }
    }

    fn other(text: &str) -> Answer {
        Answer {
            choices: Vec::new(),
            other: Some(text.to_string()),
        }
    }

    fn mixed(indices: &[usize], text: &str) -> Answer {
        Answer {
            choices: indices.to_vec(),
            other: Some(text.to_string()),
        }
    }

    #[test]
    fn blank_or_underscores_is_unanswered() {
        for raw in ["", "___", " _ "] {
            assert_eq!(
                parse_answer(&block_with_answer(SINGLE, raw)),
                Answer::default(),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn lettered_answer_selects_by_letter() {
        assert_eq!(
            parse_answer(&block_with_answer(SINGLE, "A. Echo text.")),
            choices(&[0])
        );
        assert_eq!(parse_answer(&block_with_answer(SINGLE, "B")), choices(&[1]));
        assert_eq!(
            parse_answer(&block_with_answer(SINGLE, "B) whatever")),
            choices(&[1])
        );
    }

    #[test]
    fn multi_answer_splits_on_commas() {
        assert_eq!(
            parse_answer(&block_with_answer(MULTI, "A, C")),
            choices(&[0, 2])
        );
        assert_eq!(
            parse_answer(&block_with_answer(MULTI, "C,A")),
            choices(&[0, 2])
        );
        assert_eq!(parse_answer(&block_with_answer(MULTI, "B")), choices(&[1]));
    }

    #[test]
    fn other_token_carries_free_text() {
        assert_eq!(
            parse_answer(&block_with_answer(SINGLE, "X: foobar")),
            other("foobar")
        );
        assert_eq!(parse_answer(&block_with_answer(SINGLE, "X")), other(""));
        assert_eq!(
            parse_answer(&block_with_answer(SINGLE, "X. Other (please specify)")),
            other("")
        );
        assert_eq!(
            parse_answer(&block_with_answer(MULTI, "A, C, X: foo, bar")),
            mixed(&[0, 2], "foo, bar")
        );
    }

    #[test]
    fn unmatched_text_is_treated_as_other() {
        assert_eq!(
            parse_answer(&block_with_answer(SINGLE, "custom wording")),
            other("custom wording")
        );
        assert_eq!(
            parse_answer(&block_with_answer(MULTI, "A, Z")),
            other("A, Z")
        );
        assert_eq!(
            parse_answer(&block_with_answer(FEEDBACK, "Tighten Q3")),
            other("Tighten Q3")
        );
    }

    #[test]
    fn unlettered_answer_matches_by_text() {
        assert_eq!(
            parse_answer(&block_with_answer(SUMMARY, "Looks correct")),
            choices(&[0])
        );
        assert_eq!(
            parse_answer(&block_with_answer(SUMMARY, "request changes")),
            choices(&[1])
        );
        assert_eq!(
            parse_answer(&block_with_answer(PLAN, "Approve Plan")),
            choices(&[0])
        );
    }

    #[test]
    fn render_writes_letters_for_ordinary_questions() {
        let single = block_with_answer(SINGLE, "");
        let multi = block_with_answer(MULTI, "");
        assert_eq!(render_answer(&single, &choices(&[0])), "A");
        assert_eq!(render_answer(&multi, &choices(&[2, 0])), "A, C");
        assert_eq!(
            render_answer(&multi, &mixed(&[0, 2], "foo")),
            "A, C, X: foo"
        );
        assert_eq!(render_answer(&single, &other("free text")), "X: free text");
        assert_eq!(render_answer(&single, &other("")), "X");
        assert_eq!(render_answer(&single, &Answer::default()), "");
    }

    // AI-DLC guards match these literally, so letters alone would break them.
    #[test]
    fn render_keeps_special_section_formats() {
        assert_eq!(
            render_answer(&block_with_answer(PLAN, ""), &choices(&[1])),
            "B. Request Changes"
        );
        assert_eq!(
            render_answer(&block_with_answer(SUMMARY, ""), &choices(&[0])),
            "Looks correct"
        );
        assert_eq!(
            render_answer(&block_with_answer(FEEDBACK, ""), &other("Tighten Q3")),
            "Tighten Q3"
        );
    }

    #[test]
    fn answer_line_has_no_trailing_space_when_blank() {
        assert_eq!(answer_line(""), "[Answer]:");
        assert_eq!(answer_line("A"), "[Answer]: A");
    }

    #[test]
    fn rendered_answers_satisfy_aidlc_unanswered_regex() {
        let unanswered = Regex::new(r"^\[Answer\]:[ \t]*_*[ \t]*$").unwrap();
        let block = block_with_answer(SINGLE, "");
        assert!(unanswered.is_match(&answer_line(&render_answer(&block, &Answer::default()))));
        assert!(!unanswered.is_match(&answer_line(&render_answer(&block, &choices(&[0])))));
        assert!(!unanswered.is_match(&answer_line(&render_answer(&block, &other("x")))));
    }

    #[test]
    fn toggle_single_replaces_or_clears() {
        let block = block_with_answer(SINGLE, "");
        assert_eq!(toggle_option(&block, &Answer::default(), 1), choices(&[1]));
        assert_eq!(toggle_option(&block, &choices(&[0]), 1), choices(&[1]));
        assert_eq!(toggle_option(&block, &choices(&[1]), 1), Answer::default());
        assert_eq!(toggle_option(&block, &other("x"), 0), choices(&[0]));
    }

    #[test]
    fn toggle_multi_adds_and_removes_keeping_other() {
        let block = block_with_answer(MULTI, "");
        assert_eq!(toggle_option(&block, &Answer::default(), 2), choices(&[2]));
        assert_eq!(toggle_option(&block, &choices(&[2]), 0), choices(&[0, 2]));
        assert_eq!(toggle_option(&block, &choices(&[0, 2]), 2), choices(&[0]));
        assert_eq!(toggle_option(&block, &choices(&[0]), 0), Answer::default());
        assert_eq!(
            toggle_option(&block, &mixed(&[0], "t"), 2),
            mixed(&[0, 2], "t")
        );
    }

    #[test]
    fn with_other_sets_free_text() {
        let single = block_with_answer(SINGLE, "");
        let multi = block_with_answer(MULTI, "");
        assert_eq!(with_other(&single, &choices(&[0]), "foo"), other("foo"));
        assert_eq!(with_other(&single, &other("x"), ""), Answer::default());
        assert_eq!(
            with_other(&multi, &choices(&[0]), " foo "),
            mixed(&[0], "foo")
        );
        assert_eq!(with_other(&multi, &mixed(&[0], "foo"), ""), choices(&[0]));
    }

    fn init_test(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    const ON_DISK: &str =
        "## Q1. What?\n\nA. Foo\nB. Bar\nX. Other (please specify)\n\n[Answer]:\n";

    #[gpui::test]
    async fn view_writes_answers_and_follows_disk(cx: &mut gpui::TestAppContext) {
        use fs::Fs as _;
        use project::ProjectItem as _;
        use workspace::item::ProjectItem as _;

        init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({ "intent-questions.md": ON_DISK, "README.md": "# hi\n" }),
        )
        .await;
        let project = project::Project::test(fs.clone(), [std::path::Path::new("/root")], cx).await;
        let worktree_id = cx.update(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("test project should contain a worktree")
                .read(cx)
                .id()
        });
        let project_path = |name: &str| project::ProjectPath {
            worktree_id,
            path: util::rel_path::rel_path(name).into(),
        };

        let plain = cx.update(|cx| {
            super::QuestionnaireItem::try_open(&project, &project_path("README.md"), cx)
        });
        assert!(
            plain.is_none(),
            "plain markdown must fall through to the editor"
        );

        let item = cx
            .update(|cx| {
                super::QuestionnaireItem::try_open(
                    &project,
                    &project_path("intent-questions.md"),
                    cx,
                )
            })
            .expect("questionnaire files are claimed")
            .await
            .expect("questionnaire opens");

        let (view, cx) = cx.add_window_view(|window, cx| {
            super::QuestionnaireView::for_project_item(
                project.clone(),
                None,
                item.clone(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        draw_window(cx);
        view.read_with(cx, |view, _| assert_eq!(view.progress(), (0, 1)));

        view.update_in(cx, |view, window, cx| {
            view.on_option_clicked(0, 1, window, cx)
        });
        cx.run_until_parked();
        draw_window(cx);
        let saved = fs
            .load(std::path::Path::new("/root/intent-questions.md"))
            .await
            .expect("file loads");
        assert_eq!(saved, ON_DISK.replace("[Answer]:", "[Answer]: B"));
        view.read_with(cx, |view, _| assert_eq!(view.progress(), (1, 1)));
        assert!(
            !cx.read(|cx| item.read(cx).is_dirty()),
            "save-on-change leaves no dirty state"
        );

        fs.insert_file(
            "/root/intent-questions.md",
            ON_DISK
                .replace("[Answer]:", "[Answer]: A. Foo")
                .into_bytes(),
        )
        .await;
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let block = view.question(0).expect("question survives reload");
            assert_eq!(parse_answer(block), choices(&[0]));
        });
    }

    fn draw_window(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
    }

    #[gpui::test]
    async fn other_option_takes_free_text(cx: &mut gpui::TestAppContext) {
        use fs::Fs as _;

        let (fs, view, cx) = open_questionnaire(cx, ON_DISK).await;

        view.update_in(cx, |view, window, cx| {
            view.on_option_clicked(0, 2, window, cx)
        });
        cx.run_until_parked();
        draw_window(cx);
        view.read_with(cx, |view, _| {
            assert!(view.is_editing(0), "Other opens the text input")
        });
        let untouched = fs
            .load(std::path::Path::new("/root/stage-questions.md"))
            .await
            .expect("file loads");
        assert_eq!(
            untouched, ON_DISK,
            "picking Other writes nothing until confirmed"
        );

        view.update_in(cx, |view, window, cx| {
            view.text_input.update(cx, |editor, cx| {
                editor.set_text("  something else  ", window, cx);
            });
            view.commit_editing(window, cx);
        });
        cx.run_until_parked();
        draw_window(cx);
        let saved = fs
            .load(std::path::Path::new("/root/stage-questions.md"))
            .await
            .expect("file loads");
        assert_eq!(
            saved,
            ON_DISK.replace("[Answer]:", "[Answer]: X: something else")
        );
        view.read_with(cx, |view, _| {
            assert!(!view.is_editing(0));
            let block = view.question(0).expect("question");
            assert_eq!(parse_answer(block), other("something else"));
        });
    }

    async fn open_questionnaire<'a>(
        cx: &'a mut gpui::TestAppContext,
        text: &str,
    ) -> (
        std::sync::Arc<fs::FakeFs>,
        gpui::Entity<super::QuestionnaireView>,
        &'a mut gpui::VisualTestContext,
    ) {
        use project::ProjectItem as _;
        use workspace::item::ProjectItem as _;

        init_test(cx);
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/root", serde_json::json!({ "stage-questions.md": text }))
            .await;
        let project = project::Project::test(fs.clone(), [std::path::Path::new("/root")], cx).await;
        let worktree_id = cx.update(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("test project should contain a worktree")
                .read(cx)
                .id()
        });
        let path = project::ProjectPath {
            worktree_id,
            path: util::rel_path::rel_path("stage-questions.md").into(),
        };
        let item = cx
            .update(|cx| super::QuestionnaireItem::try_open(&project, &path, cx))
            .expect("questionnaire files are claimed")
            .await
            .expect("questionnaire opens");
        let (view, cx) = cx.add_window_view(|window, cx| {
            super::QuestionnaireView::for_project_item(project.clone(), None, item, window, cx)
        });
        cx.run_until_parked();
        (fs, view, cx)
    }

    #[gpui::test]
    async fn font_size_actions_follow_markdown_preview(cx: &mut gpui::TestAppContext) {
        use settings::Settings as _;

        let (_fs, view, cx) = open_questionnaire(cx, ON_DISK).await;
        view.update_in(cx, |view, window, cx| window.focus(&view.focus_handle, cx));
        draw_window(cx);
        let size = |cx: &mut gpui::VisualTestContext| {
            cx.read(|cx| {
                theme_settings::ThemeSettings::get_global(cx).markdown_preview_font_size(cx)
            })
        };
        let before = size(cx);

        cx.dispatch_action(zed_actions::IncreaseBufferFontSize { persist: false });
        assert_eq!(size(cx), before + gpui::px(1.0));
        cx.dispatch_action(zed_actions::DecreaseBufferFontSize { persist: false });
        assert_eq!(size(cx), before);
        cx.dispatch_action(zed_actions::IncreaseBufferFontSize { persist: false });
        cx.dispatch_action(zed_actions::ResetBufferFontSize { persist: false });
        assert_eq!(size(cx), before);
    }

    #[test]
    fn duplicate_answer_is_read_only() {
        let document = parse_questionnaire("## Q1. Why?\n\nA. a\n\n[Answer]: A. a\n\n[Answer]:\n");
        let block = question(&document.sections[0]);
        assert_eq!(
            block.read_only,
            Some(super::ReadOnlyReason::DuplicateAnswer)
        );
        assert_eq!(
            block.answer.as_ref().map(|answer| answer.raw.as_str()),
            Some("A. a")
        );
    }
}
