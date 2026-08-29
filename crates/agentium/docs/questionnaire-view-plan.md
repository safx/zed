# AI-DLC 質問票ビュー (QuestionnaireView) 実装プラン

Rev 5 (2026-08-29)

改訂履歴:

- Rev 5: 3 巡目レビューを反映。フェンスの終了判定に「記号数が開始以上」を追加 (Minor)。3 巡目で
  Major 以上の指摘は無し。
- Rev 4: 2 巡目レビューを反映。フェンスコードブロックと複数行 HTML コメントの内側を不活性行として
  除外 (§3.3 手順 0)、editing 切り替え前の同期的 `commit_editing` を必須化 (§3.5/§3.6)、
  §3.8 フォールバック経路の `add_item` 置き換え挙動を注記、テスト項目を追加。
- Rev 3: 敵対的レビュー (AI-DLC 側) を反映。番号だけの見出し (`## Q1`) は最初の本文行を質問文として
  種別判定に使う (Plan Approval がその形式で出現する)、複数行回答は読み取り専用、fingerprint 空値の
  表示、Build-and-Test loop-back の不変条件を §6 に追記、§2 の根拠行番号を修正。
- Rev 2: 敵対的レビュー (Zed/GPUI 側) を反映。`project::ProjectItem::is_dirty` キャッシュを全 `BufferEvent`
  で更新 (`Saved` で陳腐化するため)、脱出口の Editor を右 pane に開く (同一 pane は entry id 重複判定で
  捨てられる)、自由文入力欄を 1 つに統一、`ElementId` を 2 要素タプルに、`menu::Confirm` に修正、
  `SaveOptions` の扱いを明記。
- Rev 1: 初版。

## 1. 目的とスコープ

Agentium で `*-questions.md` (AI-DLC の質問票) を Cmd+P 等から開いたとき、Editor ではなく
フォーム形式の独自ビューで表示し、選択肢のクリックや自由入力で `[Answer]:` 行を書き戻す。

スコープ内:

- `*-questions.md` を claim する `project::ProjectItem` + `workspace::Item` の追加
- 行ベースのパーサ (質問 / 特殊セクション / それ以外の Markdown)
- 単一選択 / 複数選択 / Other 自由入力 / 特殊セクション 4 種の入力 UI
- 回答変更時の即時保存
- エージェント側のファイル書き換えへの追従 (バッファ自動リロード → 再パース)
- 「テキストで開く」脱出口

スコープ外 (別タスク):

- Editor で開いている質問票からフォームビューを開く逆方向の導線
- `[Answer]:` 以外の行 (質問文・選択肢) の編集
- 質問票の新規作成、AI-DLC の audit log (`aidlc-log.ts`) 連携

## 2. 確認済みの前提

Zed / Agentium 側:

| 事実 | 根拠 |
|---|---|
| `Workspace::open_path` は `ProjectItemRegistry::open_path` で登録ビルダーを逆順に試し、最初に `Some` を返したものを使う | `crates/workspace/src/workspace.rs:924-940` |
| `project::ProjectItem::try_open(project, path, cx) -> Option<Task<Result<Entity<Self>>>>`。`None` で次のビルダーへ | `crates/project/src/project.rs:190-201` |
| Editor の後に登録した item が優先。`ReviewView` が `.review.json` だけを claim する前例 | `crates/agentium/src/main.rs:1431-1433` (`editor::init` が `crates/editor/src/editor.rs:354` で `register_project_item::<Editor>` を呼ぶ), `crates/agentium/src/review_view.rs:176-188` |
| `workspace::ProjectItem::for_project_item(project, pane: Option<&Pane>, item, window, cx) -> Self` | `crates/workspace/src/item.rs:1230-1238` |
| Pane 内の item 重複判定は型を見ず `Singleton` + entry id のみ | `crates/workspace/src/pane.rs:1119-1160, 1294-1375`, `crates/workspace/src/workspace.rs:5008-5032` |
| `Buffer::did_save` は `Saved` のみ emit (`DirtyChanged` は出ない) | `crates/language/src/buffer.rs:1556-1570` |
| GPUI の `cx.emit` は effect を積むだけで購読者を同期呼び出ししないので、クリックハンドラ内の `buffer.update` → `Edited` 購読で自分の entity に再入しない | `crates/gpui/src/app/context.rs` `emit`、`crates/gpui/src/app.rs` `flush_effects` (レビュアー検証) |
| `Item` の既定は `can_save=false`、`save`/`save_as`/`reload` は `unimplemented!` | `crates/workspace/src/item.rs:294-330` |
| Cmd+S は `Arena::save_active_item` → `Pane::save_item` → `item.save(...)` | `crates/agentium/src/arena.rs:199-217`, `crates/workspace/src/pane.rs:2294` |
| `Project::open_buffer(path, cx) -> Task<Result<Entity<Buffer>>>`, `Project::save_buffer(buffer, cx) -> Task<Result<()>>` | `crates/project/src/project.rs:3259, 3437` |
| `Buffer::edit(edits, None, cx)`、`is_dirty()`、`has_conflict()` | `crates/language/src/buffer.rs:2747, 2430, 2452` |
| ディスク上のファイルが変わると、バッファが clean なら `ReloadNeeded` → Project が自動リロード。dirty なら conflict になる | `crates/language/src/buffer.rs:1700-1730`, `crates/project/src/project.rs:3999-4003` |
| `Markdown::new(source, language_registry, fallback_lang, cx)`、`Markdown::reset`、`MarkdownElement::new(entity, style)`。部分描画 API はない | `crates/markdown/src/markdown.rs:649, 1045, 1643` |
| `markdown_preview` の独自パーサは削除済み (commit `9efe3c5a21`)。流用不可 | `crates/markdown_preview/src/` は 3 ファイルのみ |
| radio 部品は `ui` crate にない。`Checkbox::new(id, ToggleState).on_click(...)` はある | `crates/ui/src/components/toggle.rs:60, 90`, `crates/ui/src/traits/toggleable.rs:12` |
| 単一行入力は `Editor::single_line(window, cx)` + `EditorEvent::Blurred` 購読 + `menu::Confirm` 捕捉。リポジトリ全体で「同時に有効な入力欄は 1 つ」のパターンのみで、リスト内に複数の `single_line` を同時に置く前例は無い | `crates/agentium/src/agentium.rs:337-360, 4602`, `crates/menu/src/menu.rs:18` |
| `ElementId` への `From` は 2 要素タプルまで (`(&'static str, usize)`, `(SharedString, usize)` 等)。3 要素タプルは無い | `crates/gpui/src/window.rs:6733-6763` |
| `Item::save(&mut self, options: SaveOptions { format, force_format, autosave }, project, window, cx)` | `crates/workspace/src/item.rs:40-44, 301` |
| `Editor::for_buffer(buffer, Some(project), window, cx)`、`Workspace::open_project_item::<T>(pane, item, ...)` | `crates/editor/src/editor.rs:1797`, `crates/workspace/src/workspace.rs:5048-5058` |
| agentium のテストは `#[test]` の純関数テストのみ (dev-dependencies なし) だった。実装時に test-support 付き dev-dependencies と `#[gpui::test]` を追加した (§8)。`use super::*` は `gpui::test` を shadow するので個別 import | `crates/agentium/src/review_view.rs:1278-1279` |

AI-DLC 側 (`/Users/mac/src/tmp/aidlc-workflows`):

| 事実 | 根拠 |
|---|---|
| 質問は `## Q<n>. 見出し`、選択肢 `A.`〜`E.` + `X. Other (please specify)`、`[Answer]:` 行。見出し ID の正規表現は `^Q([1-9][0-9]*)(?:[.:](?:[ \t]+.*)?)?$` で、`## Q1` (本文行に質問文) や `## Q1: text` も正規 | `core/aidlc-common/protocols/stage-protocol.md:313-328` (選択肢と `[Answer]:`)、`core/tools/aidlc-lib.ts:5091-5094` (`visibleQuestionId`)、`tests/fixtures/intent-grounding/passing/intent-capture-questions.md:9` |
| 複数選択は見出しに `(select all that apply)`、回答は `[Answer]: A, B, E` | 同 `:328` |
| 未回答判定は `/\[Answer\]:[ \t]*_*[ \t]*$/m` (空か `_` のみ)。ディスク上のファイルを見る | `core/hooks/aidlc-continue-workflow.ts:537`, `core/tools/aidlc-sensor-claim-sources.ts:298-301` |
| 質問 ID は `^Q(\d+)\b`、H2 は `^ {0,3}##(?:[ \t]+|$)(.*)$`、`[Answer]:` は `^\[Answer\]:\s*(.*)$` (単一行、`(.*)$`)。同一質問内の `[Answer]:` 重複は違反 | `core/tools/aidlc-sensor-claim-sources.ts:78-82, 403, 414, 430` |
| `[Answer]:` の値を読む正規表現はすべて単一行末尾アンカー。複数行回答を禁止する記述は無いが、2 行目以降はどのツールも読まない | `aidlc-lib.ts:5243`, `aidlc-testing-posture.ts:96`, `aidlc-continue-workflow.ts:537`, `aidlc-sensor-claim-sources.ts:414, 430` |
| `## Consolidated Summary Confirmation`: 選択肢は文字なし箇条書き `- Looks correct` / `- Request changes`。回答は bare token。文字・番号接頭辞は無効。セクション内 `[Answer]:` はちょうど 1 つ | `stage-protocol.md:403-416`, `core/tools/aidlc-lib.ts:4824-4825, 5228-5250` |
| `## Requested Changes Feedback`: 自由文。同名セクションが複数回追記される | `stage-protocol.md:437-446` |
| `## Assumption Confirmation`: `[assumption]` 付き箇条書き + `A. Accept assumptions` / `B. Convert to follow-up questions`。回答は `A. Accept assumptions` | `core/aidlc-common/stages/ideation/intent-capture.md:137-157` |
| `## Plan Approval`: `[Approval Fingerprint]: sha256:<64hex>` 行 (値は空でもよい) + `Approve Plan` / `Request Changes`。回答判定は文字接頭辞を任意扱い (`APPROVE_PLAN_RE`)。見出しは `## Q1: Plan Approval`、`## Question 1 - Plan Approval`、さらに `## Q1` + 本文行 `Plan Approval` / `**Plan Approval**` でも正規 | `core/tools/aidlc-testing-posture.ts:95-103, 776-856`, `tests/unit/t265-plan-approval-guard.test.ts:251-273` (見出し形式)、`tests/integration/t135-invoke-swarm.test.ts:314-318` (fingerprint + 選択肢の例) |
| Build-and-Test の loop-back 中は Plan Approval の `[Answer]:` を空に戻してはならない | `core/aidlc-common/stages/construction/code-generation.md:219-226` |
| `## Sources` は `[Answer]:` を持たない (fixture 全体で確認) | `tests/fixtures/intent-grounding/passing/intent-capture-questions.md` |
| TS 側は HTML コメントを除去してから判定する | `tests/unit/t299-testing-posture-wiring.test.ts:586-590` |
| chat モードでは `[Answer]:` に timestamp と `**Mode:** chat` が付く (書式は未規定) | `stage-protocol.md:468` |

## 3. 設計

### 3.1 配置と登録

- 新規ファイル `crates/agentium/src/questionnaire_view.rs` 1 本にパーサ・item・view・action・tests を置く
  (`review_view.rs` と同じ構成。`mod.rs` は作らない)。
- `crates/agentium/src/agentium.rs` に `mod questionnaire_view;` と `pub use questionnaire_view::QuestionnaireView;` を追加。
- `crates/agentium/src/main.rs` の `register_project_item::<agentium::ReviewView>(cx);` の直後に
  `workspace::register_project_item::<agentium::QuestionnaireView>(cx);` を追加
  (Editor より後に登録されるので先に判定される)。

### 3.2 型

```rust
pub struct QuestionnaireItem {           // project::ProjectItem
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    buffer: Entity<language::Buffer>,
}

pub struct QuestionnaireView {           // workspace::Item + ProjectItem + Render + Focusable
    item: Entity<QuestionnaireItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    document: QuestionnaireDocument,       // 最新パース結果
    markdown_chunks: Vec<Entity<Markdown>>,// document.sections の Markdown 部と 1:1
    text_input: Entity<Editor>,            // Other / 自由文入力。常に 1 つだけ
    editing: Option<EditingTarget>,        // どのセクションの自由文を編集中か
    _subscriptions: Vec<Subscription>,
}

struct EditingTarget {
    kind: QuestionKind,
    ordinal: usize,   // 同種セクション内で何番目か (Feedback は同名複数のため)
}

struct QuestionnaireDocument { sections: Vec<Section> }

enum Section {
    Markdown { text: String },
    Question(QuestionBlock),
}

struct QuestionBlock {
    heading_row: u32,
    title: String,                 // 表示用 (見出し + 番号だけの見出しなら本文の質問文)
    kind: QuestionKind,
    options: Vec<ChoiceOption>,
    answer: Option<AnswerLine>,    // `[Answer]:` 行が無い場合 None
    answer_tail_rows: Vec<u32>,    // `[Answer]:` より後の非空行 (複数行回答・メタデータ)
    read_only: Option<ReadOnlyReason>,
    fingerprint: Option<Fingerprint>, // Plan Approval のみ
    body_rows: Vec<u32>,           // 選択肢・[Answer] 以外の本文 (Markdown 描画用)
}

enum ReadOnlyReason { DuplicateAnswer, MultiLineAnswer }

struct Fingerprint { row: u32, value: Option<String> } // 値は空でもよい

enum QuestionKind {
    Single,                 // Q<n>. で (select all that apply) 無し
    Multi,                  // Q<n>. で (select all that apply) 有り
    SummaryConfirmation,    // Consolidated Summary Confirmation
    Feedback,               // Requested Changes Feedback (自由文のみ)
    AssumptionConfirmation, // Assumption Confirmation
    PlanApproval,           // Plan Approval
}

struct ChoiceOption {
    row: u32,
    letter: Option<char>,   // `A.`〜`Z.` / `X.`。箇条書きは None
    text: String,           // 接頭辞と引用符を除いた本文
    is_other: bool,         // letter == 'X' または本文が Other (please specify)
}

struct AnswerLine { row: u32, raw: String } // `[Answer]:` の後ろ (trim 済み)

enum ParsedAnswer {
    Unanswered,
    Choices(Vec<char>),      // 単一なら長さ 1
    Other(String),           // 選択肢に一致しない自由文
}
```

行番号 (`row`) を保持し、バッファのイベントごとに全体を再パースする。Anchor は使わない
(ファイルは高々数百行、再パースは O(行数) で十分速い。編集は `[Answer]:` 行の丸ごと置換のみなので
行番号のずれは起きない)。

### 3.3 パース規則

入力: `buffer.read(cx).text()` を `\n` で分割した行列。書き戻しには元の行を使う。

0. 前処理 (行ごとの「不活性」判定)。次の行は H2・選択肢・`[Answer]:`・fingerprint のどの判定にも
   かけず、そのまま本文 (Markdown) として扱う:
   - フェンスコードブロックの内側。開始は `^ {0,3}(`{3,}|~{3,})(.*)$` に一致する行
     (バッククォート fence は info 文字列にバッククォートを含まないこと)。文字種と記号数を記録し、
     終了は同じ文字種で記号数が開始以上、かつ記号の後が空白のみの行
     (`aidlc-lib.ts:17180-17194` と同じ。4 個以上の fence の中に 3 個の fence 例を入れ子にした
     書式説明で、内側で誤って閉じないため)。未閉鎖なら EOF まで不活性。開始行・終了行自体も不活性
   - 複数行 HTML コメントの内側。`<!--` を含み同じ行に `-->` が無い行から、`-->` を含む行まで
   - 単一行の `<!-- ... -->` は判定前に除去する (AI-DLC の TS 側と同じ)
   AI-DLC 側は同じ扱いをしている (`tests/unit/t265-plan-approval-guard.test.ts:293-305` で
   コメント内・フェンス内の `## Plan Approval` は無視される。`aidlc-lib.ts:16897` に
   「フェンス内の見出しは教示用の例なので飛ばす」の注記)。これを省くと、書式説明のフェンスや
   コメントアウトされた旧セクションが幻の質問カードになり、クリックでその中の `[Answer]:` を
   書き換えてしまう。
1. H2 判定: `^ {0,3}##(?:[ \t]+|$)(.*)$`。末尾の ` #+` を除いて trim → `heading`。
2. `heading` が番号だけ (`NUMBERED_QUESTION_HEADING` `^(?:q(?:uestion)?[ \t]*)?\d+[ \t]*[.:)-]?[ \t]*$`)
   なら、セクション内の最初の非空行を `question_text` として取り出し (前後の `**` / `__` / `*` / `_`
   を剥がす)、その行は本文から除く。番号だけでなければ `heading` から
   `QUESTION_PREFIX` `^(?:(?:q(?:uestion)?[ \t]*)?\d+[ \t]*[:.)-][ \t]*)` を除いたものを `question_text` とする。
   `question_text` の末尾 `?` `:` を除いた `label` で種別を決める (大文字小文字は区別しない):
   - `label == "Consolidated Summary Confirmation"` → `SummaryConfirmation`
   - `label == "Requested Changes Feedback"` → `Feedback`
   - `label == "Assumption Confirmation"` → `AssumptionConfirmation`
   - `label == "Plan Approval"` → `PlanApproval`
   - `heading` が `^Q(\d+)\b` に一致 → `question_text` に `(select all that apply)` を含めば `Multi`、
     無ければ `Single`
   - それ以外 (`## Sources` など) → 質問ではない。次の H2 までを `Section::Markdown`
   - 表示用の `title` は `heading` と、番号だけの見出しなら `question_text` を連結したもの
3. 質問セクション内 (次の H2 まで) の各行:
   - `^\[Answer\]:[ \t]*(.*)$` → `AnswerLine`。2 つ目が出たら最初のものだけ採用し、
     `QuestionBlock` に `read_only = Some(ReadOnlyReason::DuplicateAnswer)` を立てる
   - `[Answer]:` 行より後にある非空行 (H2 は除く) は `answer_tail_rows` に集める。1 行でもあれば
     `read_only = Some(ReadOnlyReason::MultiLineAnswer)`。AI-DLC のツールは `[Answer]:` を
     単一行としか読まないが、複数行の自由回答や chat モードのメタデータ行が続く既存ファイルは
     あり得る。`Editor::single_line` では改行を扱えず、1 行目だけ置換すると 2 行目以降が孤立して
     矛盾したファイルになるため、このビューでは編集しない (§3.8 の Editor で編集する)
   - `^\[Approval Fingerprint\]:[ \t]*(sha256:[0-9a-f]{64})?[ \t]*$` → `fingerprint { row, value }`
     (PlanApproval 以外では Markdown 扱い)
   - 文字付き選択肢 `^([A-Z])[.)][ \t]+(.+)$` → `ChoiceOption { letter: Some(c) }`
   - 箇条書き `^[ \t]*[-*+][ \t]+(.+)$` → 種別が `SummaryConfirmation` / `AssumptionConfirmation` /
     `PlanApproval` のときだけ `ChoiceOption { letter: None }`。`Single`/`Multi` では本文扱い
     (通常質問の本文に箇条書きが混ざっても選択肢に化けないため)
   - `AssumptionConfirmation` の箇条書きのうち `[assumption]` タグを含む行は選択肢ではなく本文
     (仮定の列挙) として扱う。選択肢は文字付き行のみ
   - 本文の引用符 `"..."` / `'...'` は `text` から外す (Plan Approval の stage 定義が引用符付きのため)
   - `is_other`: `letter == Some('X')` または `text` が `Other (please specify)` で始まる
   - 残りの行は本文 (`body_rows`)。空行は無視
4. `Feedback` は選択肢を持たず、`answer.raw` をそのまま自由文入力欄に出す。
5. 回答の読み取り (`ParsedAnswer`):
   - `raw` が空または `^_+$` → `Unanswered`
   - `Multi`: `,` で分割し、各要素の先頭 1 文字が選択肢の letter に一致すれば `Choices`
   - 文字付き選択肢がある種別: `^([A-Z])[.)]?` の先頭文字が選択肢に一致すれば `Choices([c])`
   - 文字なし選択肢 (`SummaryConfirmation`, 箇条書きの `PlanApproval`): `raw` を trim・引用符除去し、
     選択肢 `text` と大文字小文字無視で一致すれば `Choices` (letter 代替として index を使う)
   - どれにも一致しない非空 → `Other(raw)`

### 3.4 書き戻し規則 (`[Answer]:` 行の全体置換)

| 種別 / 状態 | 書く内容 |
|---|---|
| 単一選択、文字付き選択肢 | `[Answer]: A. <text>` |
| 単一選択、文字なし選択肢 (`SummaryConfirmation`, 箇条書き `PlanApproval`) | `[Answer]: <text>` (bare token) |
| 複数選択 | `[Answer]: A, B, E` (選択順ではなく文字順) |
| Other 選択 + 自由文 | `[Answer]: <自由文>`。自由文が空なら `[Answer]:` のまま (未回答) |
| `Feedback` | `[Answer]: <自由文>` |
| 選択解除 (すべて外す) | `[Answer]:` |

制約:

- `[Answer]:` 行が無い質問は書き戻し対象にしない (行を新規追加すると TS 側の重複判定や
  セクション構造を壊すため)。UI では入力を無効化し「[Answer]: 行がありません」と表示する。
- `read_only` が立っている質問 (重複 `[Answer]:`、複数行回答) も書き戻さない。理由を表示し、
  「Open as text」を案内する。
- 置換範囲は `Point::new(row, 0)..Point::new(row, line_len)`。改行は触らない。
- 同一行に付いていた timestamp / `**Mode:**` メタデータは失われる。chat モードは
  エージェント主導で人がフォームを触る想定ではないので許容する (§6 に記載)。

### 3.5 レンダリング

- 全体は `v_flex().id("questionnaire").overflow_y_scroll()` で縦スクロール。
- `Section::Markdown` は `Markdown` 実体を 1 つずつ作り `MarkdownElement::new(entity, style)` で描画。
  スタイルは `MarkdownStyle::themed(MarkdownFont::Ui, window, cx)` を基準にする
  (`review_view.rs:1223` は hover 用の `diagnostics_markdown_style` を使っているが、
  ここは本文なので UI フォント基準にする)。再パースで `text` が変わらなければ `Markdown::reset`
  を呼ぶだけ (同じ文字列なら no-op) にし、実体は再生成しない。
- `Section::Question` はカード (`border_1().rounded_md().p_3()`) で:
  - 見出し行: `Label::new(title)` + 種別バッジ (`Multi` なら "select all")
  - 本文 (`body_rows`) があれば Markdown として描画
  - `PlanApproval` の fingerprint 行は等幅 `Label` で読み取り専用表示。値が空 (未計算) なら
    「fingerprint not computed」を muted で表示
  - `answer_tail_rows` があれば `[Answer]:` の下に Markdown として読み取り専用表示
  - 選択肢行: `Checkbox::new((SharedString::from(format!("q{section_idx}")), option_idx), state)`
    + `Label(text)`。`ElementId` は 2 要素タプルまでなので 3 要素にはしない。
    単一選択では他をすべて `Unselected` にする排他処理をクリックハンドラで行う
    (radio 部品が無いため。見た目の丸型化は行わない)
  - 自由文 (Other の本文、`Feedback` の本文) は通常は `Label` (空なら placeholder 文言) を
    クリック可能に描き、クリックで `editing = Some(target)` にして、その位置にだけ
    `text_input` を描いて focus する。入力欄はビュー全体で常に 1 つ
    (リポジトリの rename editor と同じ「同時に 1 つ」パターン。複数同時配置は前例が無い)
  - `is_other` の選択肢を選んだ直後は自動で editing に入る
  - **editing の切り替えは必ず「現在の editing を同期的に確定してから」行う**。Checkbox の
    クリック、別セクションの自由文クリック、Open as text など、`editing` を変更・破棄する
    すべての起点で先に `commit_editing(cx)` を呼ぶ。GPUI では `on_click` ハンドラが先に走り、
    フォーカス移動による `EditorEvent::Blurred` はハンドラ終了後の effect flush で届くため、
    先に `editing` を差し替えると Blurred ハンドラは新ターゲットの値を保存してしまい、
    旧ターゲットの未確定入力が無警告で消える
  - 右上に現在の `[Answer]:` の生の値を `Label` (muted) で表示 (書き戻し結果の確認用)
  - `answer == None` / `read_only.is_some()` のときは Checkbox を `disabled(true)` にし、理由の
    Label (「[Answer]: 行がありません」「複数行の回答はテキストで編集してください」等) を出す
- ヘッダ (ビュー最上部): ファイル名、回答済み数 / 質問数、「Open as text」ボタン
  (`OpenAsText` action を dispatch)。

### 3.6 編集・保存フロー

```
click / blur
  → QuestionnaireView::set_answer(section_idx, new_raw, cx)
    → buffer.update(cx, |b, cx| b.edit([(range, format!("[Answer]: {new_raw}"))], None, cx))
      (new_raw が空なら "[Answer]:" のみ)
    → project.update(cx, |p, cx| p.save_buffer(buffer.clone(), cx)).detach_and_log_err(cx)
  → BufferEvent::Edited → 再パース → cx.notify()
```

- Checkbox クリック: 即時に edit + save。
- 自由文入力欄 (`text_input`): editing に入るとき `set_text(現在の raw)` で初期化し focus。
  確定は `commit_editing(cx)` に集約する: `editing` が `Some` なら `text_input` の内容を
  そのターゲットへ edit + save し `editing = None`。呼び出し起点は (a) 入力欄を包む要素の
  `.on_action(cx.listener(|this, _: &menu::Confirm, ...))` (Enter)、(b) editing を切り替える
  各クリックハンドラの先頭、(c) `EditorEvent::Blurred` (クリックを介さないフォーカス喪失、
  ウィンドウ非アクティブ化の保険)。`menu::Cancel` (Esc) は破棄して `editing = None`。
  1 打鍵ごとには書かない。
  編集中にバッファ側の同セクションが変わっても入力欄は上書きしない (確定時に
  `EditingTarget { kind, ordinal }` でセクションを再特定して書き込む。見つからなければ破棄)。
- 即時保存にする理由: AI-DLC の hook (`aidlc-continue-workflow.ts:537`) と Step 3b の
  「人が編集 → done → エージェントが読む」はディスクの内容だけを見る。dirty バッファは見えない。
  加えて、clean なバッファはエージェントの書き換えで自動リロードされるが、dirty だと
  `has_conflict` になり Cmd+S で上書き確認プロンプトが出る (`pane.rs:2294` `save_item`)。
- `Item` 実装 (`workspace::Item` のメソッドは `cx: &App` を受けるので buffer を直接読む。
  `cx` の無い `project::ProjectItem::is_dirty` とは別物):
  - `is_dirty(&self, cx)` → `buffer.read(cx).is_dirty()`、`has_conflict(&self, cx)` → `buffer.read(cx).has_conflict()`
  - `can_save` → `true`、`save(options: SaveOptions, ...)` → `project.save_buffer(buffer, cx)`。
    `options.format` / `force_format` は無視する (フォーマッタが質問票を整形すると TS 側の
    正規表現判定を壊しうるため、このビューからは常に無整形で保存する)
  - `can_save_as` → 既定の `false` のまま
  - `reload` → `project.reload_buffers([buffer], true, cx)`
  - `for_each_project_item` → `QuestionnaireItem` を渡す (Pane の重複オープン判定に必要)
  - `buffer_kind` → `ItemBufferKind::Singleton`
  - `tab_content_text` → ファイル名、`tab_icon` → `IconName::FileGeneric` 等
  - `type Event = QuestionnaireEvent { Edited, TitleChanged }`、`to_item_events` で
    `Edited → ItemEvent::Edit`、`TitleChanged → ItemEvent::UpdateTab`
- `QuestionnaireItem` の `project::ProjectItem`:
  - `try_open`: `path.path.file_name().is_some_and(|n| n.ends_with("-questions.md"))` 以外は `None`。
    一致時は `project.update(cx, |p, cx| p.open_buffer(path.clone(), cx))` を await し、
    `entry_id` は `project.read(cx).entry_for_path(&path, cx).map(|e| e.id)` で得る
  - `entry_id` / `project_path` はフィールドを返す。`is_dirty` はシグネチャが
    `fn is_dirty(&self) -> bool` で `cx` が無いので、`QuestionnaireItem` 自身が `try_open` 内の
    `cx.new(|cx| ...)` で `cx.subscribe(&buffer, ...)` し、**すべての** `BufferEvent` で
    `buffer.read(cx).is_dirty()` を読み直してフィールドに保存する。`DirtyChanged` だけを見ると
    保存時に陳腐化する: `Buffer::did_save` は `Saved` のみ emit し `DirtyChanged` を出さない
    (`crates/language/src/buffer.rs:1556-1570`)。この値は `Pane` のタブ閉じ時の未保存確認
    (`pane.rs` `skip_save_on_close` / `file_names_for_prompt`) に使われるので、陳腐化すると
    保存済みでも確認ダイアログが出る

### 3.7 外部変更への追従

- `cx.subscribe(&buffer, ...)` で `BufferEvent::Edited { .. }` / `Reloaded` / `DirtyChanged` /
  `Saved` / `FileHandleChanged` を受けたら再パース → Markdown 実体の `reset` → `cx.emit(Edited)`
  → `cx.notify()` (`Edited` は構造体バリアントなので `Edited { .. }` で match する)。
- エージェントが同じファイルを書き換える → バッファが clean なら `ReloadNeeded` → Project が
  リロード → `Reloaded` → ビューが更新される。dirty (保存失敗直後など) なら conflict となり
  タブに conflict 表示。`reload` 実装で復帰できる。
- 再パースで質問の数や順序が変わっても、編集中セクションは `EditingTarget { kind, ordinal }`
  で再特定する (行番号は追記で動くため `heading_row` は使わない)。

### 3.8 「テキストで開く」脱出口

前提となる Pane の規則: **1 つの pane には同じ entry id の Singleton item は 1 つしか置けず、
判定は item の型を見ない**。

- `Pane::open_item` (Cmd+P 経路) は `buffer_kind == Singleton && project_entry_ids == [entry_id]`
  の既存 item があればそれをアクティブ化して終わる (`crates/workspace/src/pane.rs:1119-1160`)
- `Pane::add_item` も同じ条件で既存 item を探し、既存がアクティブなら新 item を挿入せず捨てる
  (`pane.rs:1294-1375`)
- `Workspace::find_project_item` は entry id で見つけた item を `downcast::<T>()` するだけなので、
  型が違えば `None` → 新規作成 → `add_item` で捨てられる (`workspace.rs:5008-5032`)

したがって「同じ pane に Editor を開く」は動かない。Editor は **右隣の pane** に開く。

- `actions!(questionnaire_view, [OpenAsText]);` を `questionnaire_view.rs` に定義。
- ビューのヘッダボタンが `window.dispatch_action(OpenAsText.boxed_clone(), cx)`。
- `Arena::render` に `.on_action(cx.listener(|this, _: &OpenAsText, window, cx| ...))` を追加
  (`Workspace` は element tree に無いので action は Arena で受ける。crate の CLAUDE.md の規則)。
  処理は `open_markdown_preview(side = true)` (`arena.rs:878-960`) と同じ手順:
  1. `active_pane.active_item().act_as::<QuestionnaireView>(cx)` からバッファを取り出す
  2. `find_pane_in_direction(Right)` で右 pane を取り、無ければ `new_agentium_pane` + `center.split`
  3. `workspace.update(cx, |ws, cx| ws.open_project_item::<Editor>(right_pane, buffer, true, true, false, false, window, cx))`
     (右 pane に同じファイルの Editor があればそれを前面化する)。
     `open_project_item` は agentium 内に前例が無い新規の組み合わせ。動かなければ
     `cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx))` + `right_pane.add_item(...)`
     に切り替える。ただし `add_item` は同じ entry id の既存 item を新 item で置き換えるので
     (`pane.rs:1294-1375`)、既存 Editor のカーソル位置などは保持されない。主経路の
     `open_project_item` は `find_project_item` で既存 Editor を返して early return するので
     この問題はない
- キーバインドは付けない (ボタンのみ)。

同じ規則の帰結として、Editor でこのファイルを開いている pane で Cmd+P から同じファイルを選ぶと
Editor がアクティブ化されフォームは出ない (逆も同じ)。これは Zed の image_viewer と Editor の
関係と同じで、仕様として受け入れる (§6 に記載)。同一 item 内で raw/form を切り替える案
(ReviewView のように Editor を内包し `act_as_type` で公開する) は後続タスクとする。

### 3.9 誤検出への対処

`try_open` はパスだけで判定するので、AI-DLC 以外の `*-questions.md` も claim する。
`## Q<n>` も特殊セクションも 0 個のファイルは全体が `Section::Markdown` 1 つになり
Markdown 表示になる。テキスト編集は §3.8 のボタンで行う。設定によるオプトアウトは今回は付けない。

## 4. 実装ステップ

各ステップは単体でビルドとテストが通る状態で終える。

0. `README.md` 先頭に `> [!IMPORTANT]` 2 行があることを確認する (リポジトリ CLAUDE.md の
   HARD RULE。2026-08-29 時点の HEAD には既にあるので、無ければ付ける)。
1. パーサとシリアライザ (純関数): `parse_questionnaire(text: &str) -> QuestionnaireDocument`、
   `parse_answer(block, raw) -> ParsedAnswer`、`render_answer(block, selection) -> String`。
   `#[test]` を同ファイルの `mod tests` に置く (`use super::*` 禁止、個別 import)。
2. `QuestionnaireItem` / `QuestionnaireView` の骨格: `try_open`、`for_project_item`、`Item`、
   全セクションを Markdown として描画するだけの `Render`。`main.rs` に登録。
   手動確認: Cmd+P で `*-questions.md` を開くとタブがフォームビューになり、`.md` は Editor のまま。
3. 通常質問の UI: Checkbox 排他、複数選択、Other 入力、書き戻し、即時保存。
   手動確認: 保存後のファイル内容が §3.4 の書式になること (`cat` で確認)。
4. 特殊セクション 4 種のディスパッチと UI (bare token 書き戻し、fingerprint 表示、Feedback 自由文)。
5. バッファイベント購読と再パース、編集中セクションの再特定、conflict 時の表示。
   手動確認: ビューを開いたまま別プロセスでファイルを書き換えると追従する。
6. `OpenAsText` action と Arena ハンドラ、ヘッダの回答済みカウント。
7. `./script/clippy` と `cargo test -p agentium` を通す。`cargo run -p agentium` で手動シナリオ (§5)。

## 5. テスト計画

単体テスト (ステップ 1):

- fixture `intent-capture-questions.md` 相当の文字列 → 質問 8 個、`## Sources` は Markdown、
  すべて `Choices(['A'])`
- 空 `[Answer]:` と `[Answer]: ____` → `Unanswered`
- `(select all that apply)` + `[Answer]: A, C` → `Multi`, `Choices(['A','C'])`
- `[Answer]: 独自回答` (どの letter にも一致しない) → `Other`
- `## Consolidated Summary Confirmation` + `- Looks correct` / `- Request changes` +
  `[Answer]: Looks correct` → `SummaryConfirmation`、選択肢 2 個 (letter None)、書き戻しが bare
- `## Plan Approval` + fingerprint + `A. Approve Plan` → `PlanApproval`、`fingerprint` が `Some` で値あり
- `## Q9. Plan Approval` / `## Q1: Plan Approval` / `## Question 1 - Plan Approval` も `PlanApproval`
- `## Q1` + 空行 + `Plan Approval` (本文行にラベル) と `**Plan Approval**` も `PlanApproval` になり、
  そのラベル行は本文に残らない (`t265-plan-approval-guard.test.ts:261-268` と同じ入力)
- `## Q2` + 本文行 `Which regions? (select all that apply)` → `Multi`
- `## Plan <!-- x -->Approval` (コメント混入) も `PlanApproval` になる
- `## Q1` + 本文 `Which checkpoint applies?` + 選択肢 `A. Plan Approval` → `Single` であり
  `PlanApproval` ではない (`t265:287-290`)
- ```` ```markdown ```` 〜 ```` ``` ```` および `~~~` フェンス内の `## Plan Approval` / `[Answer]:` は
  セクションにならず Markdown 本文になる (`t265:296-305`)
- `<!--` 〜 `-->` の複数行コメント内の `## Plan Approval` / `[Answer]:` も同様 (`t265:293-295`)。
  コメント後の本物の `## Plan Approval` は 1 つだけ検出される
- `[Answer]: A. Foo` の次の行に非空テキストがある → `read_only == Some(MultiLineAnswer)`、
  `answer_tail_rows` にその行が入る
- `[Approval Fingerprint]:` の値が空 → `fingerprint` が `Some` で `value == None`
- `## Assumption Confirmation` の `- [assumption] ...` 行は選択肢に数えない
- `## Requested Changes Feedback` が 2 回出ても両方 `Feedback` として別セクションになる
- 通常質問の本文に `- ` 箇条書きがあっても選択肢に化けない
- 書き戻し結果が TS 側の未回答正規表現 `/\[Answer\]:[ \t]*_*[ \t]*$/m` に一致しない (回答あり) /
  一致する (解除) ことをテストで固定する
- 選択肢・`[Answer]:` の間に `**Mode:** chat` などの行があってもパースできる

手動シナリオ (ステップ 3〜7):

1. Cmd+P → `intent-capture-questions.md` → フォーム表示、`.md` の他ファイルは Editor
2. 選択肢クリック → ファイルが即時更新される、タブに dirty 表示が残らない
3. X. Other → 入力欄表示 → Enter / blur で保存
4. 別ターミナルで `[Answer]:` を書き換える → ビューが追従
5. `## Q` を含まない `foo-questions.md` → 全体 Markdown 表示、Open as text で Editor
6. `[Answer]:` 行の無い質問 → 入力無効 + 警告
7. FileBrowser / GitStatus からの open も同じビューになる

## 6. リスクと未決事項

- radio の見た目が Checkbox になる。許容するか、`ToggleButtonGroup` に置き換えるかは実装後に判断。
- 見出しや選択肢本文の Markdown 装飾 (`**強調**` 等) は `Label` ではそのまま文字列表示になる。
  必要なら `Markdown::new_text` (リンクのみ解釈) で描画する。
- `[Answer]:` 行の全体置換で同じ行に付いた timestamp / `**Mode:**` メタデータが消える。
  書式が未規定のため保全しない。別行に付いている場合は複数行回答として読み取り専用になる。
- 複数行の自由回答はこのビューでは編集できない (読み取り専用 + Open as text 案内)。
- Plan Approval で選択を外すと `[Answer]:` が空になる。Build-and-Test の loop-back 中はこれを
  空に戻してはならない規定があり (`code-generation.md:219-226`)、ユーザー操作で不変条件を
  壊しうる。実装で防ぐことはせず、認識にとどめる。
- 自由文入力中にエージェントが同種セクションを挿入・削除すると `ordinal` がずれ、確定時に
  別セクションへ書き込みうる。確定前に再特定したセクションの `title` が編集開始時と一致するか
  も確認し、不一致なら破棄して通知する。
- スクロールする `v_flex` の中に `Editor::single_line` を置く構成はリポジトリに前例が無い
  (rename editor はサイドバーの固定位置)。フォーカス移動やスクロール追従に問題が出たら、
  入力欄をカード外 (ビュー下部の固定行) に置く構成に切り替える。
- `try_open` が `*-questions.md` をすべて claim する。オプトアウト設定は今回は持たない。
- 同じ pane に同じファイルの Editor が既に開いていると Cmd+P はその Editor を前面化し、
  フォームは出ない (Pane の entry id 重複判定、§3.8)。Editor を閉じて開き直せばフォームになる。
- GPUI テスト基盤は実装時に追加した (§8)。`project/test-support` を有効にすると `remote/test-support`
  が `RemoteConnectionOptions::Mock` を追加するが、`remote_connection` 側の match arm は自身の
  `test-support` でしか有効にならず E0004 になる。`remote_connection = { features = ["test-support"] }`
  を dev-dependency に足す (git_ui と同じ対処)。

## 7. 見積り

- パーサ + シリアライザ + tests: 300〜400 行
- item / view / render / action: 500〜600 行
- main.rs / agentium.rs / arena.rs の変更: 30 行以内
- 合計 900〜1000 行、`review_view.rs` (1429 行) より小さい規模。

## 8. 実装メモ (2026-08-29)

Kent Beck 流 TDD (Red → Green → Refactor) で `crates/agentium/src/questionnaire_view.rs` に実装した。

- 純関数層 (`parse_questionnaire` / `parse_answer` / `render_answer` / `answer_line` / `toggle_option`) は
  `#[test]` 33 本をテストファーストで書き、各サイクルで Red を確認してから実装した。
- GPUI 層は `#[gpui::test]` 2 本で検証した (`FakeFs` + `Project::test` + `add_window_view`):
  `view_writes_answers_and_follows_disk` (try_open のフィルタ、選択肢クリック → ディスク内容、dirty 解消、
  外部書き換え → リロード → 再パース、`window.draw` のスモーク) と `other_option_takes_free_text`
  (X. Other → 入力欄 → 確定 → 自由文の書き戻し)。
- プランからの差分: `QuestionBlock` の本文は行番号ではなく結合済み `String` (`body`, `answer_tail`) を持つ
  (view が行列を持ち回る必要がなくなるため)。`Fingerprint` は `{ row, value: Option<String> }`。
  `QuestionnaireEvent` は `Edited` のみ。
- 未実施: 実機 GUI での目視確認。osascript のキー送信が Accessibility 権限で拒否されたため自動化できず、
  Cmd+P からの表示・レイアウト・「Open as text」ボタンの動作は人手での確認が必要。
- 実機フィードバック (2026-08-29): 実際の質問票は `## トピック` の下に `### Q1.` と H3 で質問を書いていた
  (H2 が無いファイルもあり得る)。§3.3 の「H2 判定」は **見出しレベル 1〜6 すべてを区切りとして扱い、
  質問かどうかは見出し文言だけで判定する** に変更した。連続する Markdown セクションは 1 チャンクに結合する。
  選択肢行は「A. 本文」の 1 行表示、行全体クリックで選択、長文は折り返し。
- 実機フィードバック 2 回目 (2026-08-29): 通常質問 (`Single` / `Multi`) の書き戻しを **文字だけ** に変更した
  (`[Answer]: A`、複数は `[Answer]: A, C, X: foobar`、Other 単独は `X: foobar`)。読み取り時に
  この規則に合わない値は X (Other) 扱いにして X をチェックし、自由文欄に表示する。
  **特殊セクションは従来書式のまま** (`A. Approve Plan`、`Looks correct` など): AI-DLC の
  `APPROVE_PLAN_RE` / `summaryConfirmationAnswer` が文言を要求し、文字だけでは承認と認識されないため。
  選択中の選択肢は bold、質問右上の回答表示は回答済みなら緑 (`Color::Success`)。
  回答モデルは `Answer { choices: Vec<usize>, other: Option<String> }` に置き換えた (§3.2 の `ParsedAnswer` 列挙は廃止)。
