# Task Overview カラムと To do の item モデルの実装プラン

Rev 6 (2026-11-09)

改訂履歴:

- Rev 6: 段 2 の実装で決めた 4 点。(1) `todo take` は担当に加えたうえで `Requested` にする (AI が自分で
  やる意思表示なので、自分で `add` した場合と同じ)。(2) task に属さない item は、その arena の path を
  含むすべての scope に出る。(3) `todo list` の範囲は呼び出し元 arena の task (選択中の task を優先)
  で、各 item に呼び出し元が担当かどうか (`mine`) を付ける。(4) Go は担当セッション全員に 1 通ずつ送り、
  全員に届いた item だけ `Requested` にする。閉じた worktree 行の To do 数ピルは外し、task 行の件数に
  含める (item は task 単位で path ごとに数えられないため) (§3.1 / §3.2 / §3.3 / §3.4)。

- Rev 5: 段 1 の実装で判明した 2 点。(1) 質問節の独立化 (§3.4) は段 1 から段 3 に移す。段 1 では
  エージェントカードが残るので質問は今までどおりカードの中で描画され、カードを消す段 3 で独立させる
  のが自然なため (§4)。(2) サイドバーの task 行のドットは、arena 行のバッジと同じ端末メニューを開く
  (クリックで task を選び、その状態を持つ最初の arena に切り替えてメニューを出す)。件数は全 arena 分、
  メニューの項目は最初の arena 分 (§3.1)。

- Rev 4: Codex 指摘 1 件を反映。Done の item に必須なのは `resolution` だけで、`request` は Requested を
  経た item だけが持つ。Open から直接 Skip / Done する既存の遷移を残す (§3.2 / §5)。

- Rev 3: Codex 再レビューの must-fix 3 件を反映。(1) scope の照合は `task_id` を正とし、path / 名前の
  fallback は `task_id` が `None` の item にだけ使う (§3.2 / §5)。(2) Go のときの人の条件 (メモ) を
  `request` として item に残し、Requested の行に表示する。`resolution` は閉じた記録に限る (§3.2 /
  §3.4)。(3) 段 2 は model + 移行 + UI + CLI + 文書を 1 回で atomic にマージする。レビューのために
  diff を積んで分けるのはよいが、連続 PR で中間状態を置く選択肢は消す (§4 / §6)。

- Rev 2: Codex プランレビューの must-fix 9 件を反映。(1) todo コマンドの transport は CLI stream socket
  (`cli_request`、`agentium-cli.sock`) のままにする。datagram と 1900 バイト上限は task command の話で
  todo には関係ない (§2 / §3.3)。(2) 長さ上限は transport の都合ではなく保存と表示の都合として置く
  (§3.3)。(3) Overview にフォーカス中のキー入力: `OpenNewItemMenu` / `OpenSplitMenu` / `NewTerminal`
  も `Agentium` ルートで委譲し、`ctrl-~` 等の `Workspace` 文脈バインディングは `Agentium` 文脈にも
  登録する。Overview のメモ欄から実キー入力で検証する (§3.1 / §5)。(4) `menu::Confirm` / `Cancel` の
  単一ディスパッチャをサイドバー子要素から AgentiumApp のルートに上げる (§3.1)。(5) 旧 list の
  `arena_name` / `branch` と、path の無い list の名前照合を item に引き継ぐ (§3.2)。(6) 担当者ごとに
  復元用の arena を持つ `Assignee` を導入する (§3.2)。(7) Done の内訳 (完了 / Skip / 理由) を永続化する
  `resolution` を足す (§3.2)。(8) 質問の表示と件数は To do 行と独立に保つ (§3.1 / §3.4)。(9) 段 2 の
  「`set` を内部で item に写す」シムは旧所有者モデルと両立しないので廃し、item モデルと新 CLI と文書を
  1 段にまとめる (§4)。
- Rev 1: 初版。2026-11-06 の「1 arena = 1 PaneGroup の制約を崩せるか」の調査と、2026-11-08 のモック
  (サイドバーのタスク 1 行化、Task Overview の issue / Arena / PR / To do 一覧) に対する決定を反映。

## 1. 目的とスコープ

Task Overview を arena のタブから出し、サイドバーの右の独立したカラムに置く。サイドバーは task 1 行に
畳む。To do は「タブ (asker) が持つ list を `todo set` で丸ごと置き換える」宣言型をやめ、task に属する
item の集合にする。item には担当セッションが 0 件から複数付き、担当者は人でも AI でも UI でも CLI でも
変えられる。Software factory の文脈で、To do を task の作業単位として扱うための土台を作る。

得たいもの:

- arena を切り替えても Task Overview が消えない。
- task に arena が 2 つあっても、新規タブがどの arena に行くかが入口ごとに決まる。
- To do は依頼直後に消えず、担当 AI が `todo done` で閉じるまで残る。Pick (Proposal) が上、
  You do (UserTask) が下に並ぶ。
- AI が「自分の担当ではない」と判断した To do と、人が CLI で作る To do を未割り当てで持てる。

やらないこと:

- 1 arena = 1 PaneGroup の構造は変えない。別 arena のタブを 1 つのペイン群に混ぜない (2026-11-06 の案 1)。
- `todo ask` と質問 (`inbox_questions`) の仕組みは触らない。表示位置だけ §3.4 で変える。
- 担当者の変更と `take` 以外のディスパッチやキュー (自動割り当て、優先度) は作らない。

決定済み (2026-11-08):

1. Overview はサイドバーの右のカラム。サイドバー | Overview | arena のペイン群の 3 カラム。
2. `done` / `update` は担当者だけ。担当者の変更 (`assign` / `take` / `unassign`) は人でも AI でも
   UI でも CLI でも可。これは事故防止の作業規約であって境界ではない (AI は `assign self` の後に
   `done` できる)。
3. To do は AI も人も CLI で作る。AI は `--unassigned` で自分を担当から外せる。
4. `todo set` / `todo clear` は削除する。シムは置かない。
5. エージェントカードの request / response は To do の担当セッション行の下に移す。コーディネーターは
   `add` と `assign` ができる 1 セッションとして扱う。

## 2. 確認済みの前提

- 描画階層は `AgentiumApp` → `Arena` → `PaneGroup` → `Pane`。中央は `active_arena()` 1 つだけを描画する
  (`agentium.rs:9808-9819`)。`Arena` は `center: PaneGroup` と `active_pane` を所有する (`arena.rs:76-88`)。
- `Workspace` と `Project` は全 arena で 1 つを共有する (`main.rs:3118-3136`、`agentium.rs:1615`)。
  レイアウトは永続化されていない (`Workspace::new(None, ..)`)。
- `activate_context` (`arena.rs:337-354`) が git store の active repository を切り替える。ライブに読むのは
  `git_status_view.rs:160-161` だけ。中央が arena 1 つのままなので今回は触らない。
- Task Overview は今、active arena の active pane に Item として追加される (`open_overview_scope`、
  `agentium.rs:7245-7273`)。同じ scope の既存タブを全 arena から探してから作る。
- `handle_tab_new` (`agentium.rs:2874-2903`) は、描画されていない item に focus すると見えている arena が
  キー入力を失うため、非アクティブ arena には端末以外のタブを追加しない。
- キー入力: `cmd-t` → `OpenNewItemMenu`、`cmd-shift-t` → `OpenSplitMenu` は `Workspace` と `Agentium`
  の両文脈に登録されている (`main.rs:3074-3085`) が、ハンドラは `Arena::render` にしかない
  (`arena.rs:1566`、`1574`)。`ctrl-~` → `workspace::NewTerminal` は Zed 既定キーマップの `Workspace`
  文脈 (`assets/keymaps/default-macos.json:717`)。サイドバーの名前変更 / issue 入力の `menu::Confirm` /
  `menu::Cancel` はサイドバーの子要素 1 か所で分岐する (`agentium.rs:9601-9624`)。
- To do の現状: `InboxTodoList` は asker タブごとに 1 つ (`agentium.rs:461-480`)。`arena_path` と
  `branch` は後付けで省略可 (`469-474`)。item は kind / title / detail だけで id を持たない
  (`361-365`)。title の一意性は list 内だけ (`369-370`)。所有者の照合は live のタブを先に、次に asker で
  引くので、asker の無い素の端末や Codex 端末タブも自分の list を持てる (`todo_list_index`、
  `484-496`)。送信時に `todos.send` と `record_processed` で item が一覧から消え、Processed 節が
  choice と note を保持する (`3486-3495`、`inbox_todo_view.rs:573-660`)。path の無い旧 list は
  `arena_name` で照合・復元する (`task_overview.rs:170-173`、`inbox_todo_view.rs:155-181`)。
  `todo.json` はトップレベルが list の配列 (`inbox_todo_view.rs:487`)。
- `todo set` / `todo list` / `todo clear` / `todo ask` は CLI stream socket 経由 (`cli_request`、
  `main.rs:786`、呼び出し `1017` / `1035`)。呼び出し元の ancestry は `UnixStream` の peer から取る
  (`main.rs:2022`、`2092`)。本体側は `tab_for_ancestors` で Agentium の端末か ACP タブ以外を拒否する
  (`agentium.rs:3007`、`3213-3332`)。`inbox_asker` (`3501-3516`) は Claude セッション ID か Codex ACP
  の saved id を返し、素の shell では `None`。`InboxAsker` はセッション ID だけで arena を持たない
  (`517-528`)。
- datagram socket (`agentium.sock`、`main.rs:2344`) と 1900 バイトの上限 (`task_command_envelope`、
  `main.rs:1866` / `1908`) は `agentium arena new` と task command の経路で、todo には使っていない。
- サイドバーの Tasks タブは task の下に issue 行と arena 行を畳んで出す (`render_task_list`、
  `agentium.rs:9346-9373`)。arena ごとのバッジは `collect_terminal_infos_for_state` (`5252`)、To do の
  バッジ数は item と未回答の質問を合わせて数える (`7877-7879`)。ブランチ名は
  `branch_name_for_arena` (`4387`)。
- Overview の質問 UI は各エージェントカードの中で描画され (`render_agent_todos`、`7616-7627`)、
  カード外の fallback は閉じた持ち主の質問だけ (`7643-7650`)。
- `skills/agentium-inbox/SKILL.md` は稼働中の Claude / Codex が読むスキルへの symlink。コミットした瞬間に
  両方に効く。

## 3. 設計

### 3.1 レイアウト

- `AgentiumApp` に `selected_task: Option<Uuid>` と、task ごとの最後にアクティブだった arena
  (`last_active_arena: HashMap<Uuid, EntityId>`) を足す。
- `AgentiumApp::render` は サイドバー | Overview カラム | 中央 の順に 3 子を並べる。Overview カラムは
  `Entity<TaskOverview>` を直接描画する。Pane もタブも置かない。幅は固定 (暫定 400px) で折りたたみ
  ボタンを付ける。
- `TaskOverview` は `OverviewScope::Task` と `OverviewScope::Arena` (task に属さない arena) のまま。
  `impl Item for TaskOverview` と `open_task_overview` / `open_overview_scope` / `NewTaskOverview`
  アクションは削除する。既存タブ探索 (`7251-7258`) も消える。
- サイドバーのタスク行クリック: `selected_task` を更新し、その task の `last_active_arena` (無ければ
  先頭の Live arena) に `switch_arena`。`switch_arena` は逆に、その arena を含む task を `selected_task`
  にする (2 つの task に属す arena は、今の `selected_task` に含まれればそれ、なければ
  `tasks_containing_worktree` の先頭)。
- サイドバーは task 1 行 (タイトル、状態ドット、To do 数のピル) と Unassigned arena 節だけ。
  Repositories タブと `SidebarTab` を削除する。arena 行の名前変更、PR / CI 表示、コンテキストメニューは
  Overview の Arena 一覧に移す。`cmd-1`〜`cmd-9` (`ActivateArena`) は task 番号に変える
  (`ActivateTask { index }`)。
- 状態ドットは task の Live arena 全部の `collect_terminal_infos_for_state` を OR した有無で出す
  (件数は出さない)。ドットのツールチップとクリックは arena 行のバッジと同じで、クリックで task を選び、
  その状態を持つ最初の arena に切り替えて端末メニュー (`deploy_badge_menu`) を開く。To do 数は task の Open / Assigned / Requested の item 数と、task の arena に属する
  未回答の質問数の和。今の `7877-7879` と同じ数え方を item に置き換えるだけで、質問は落とさない。
- キー入力の委譲: `Agentium` 文脈のルート要素に `OpenNewItemMenu` / `OpenSplitMenu` / `NewTerminal` /
  `NewClaudeCode` / `NewCodexAcp` / `NewDiffView` などの `on_action` を足し、`active_arena()` に委譲する。
  `Arena::render` の既存ハンドラはそのまま (`arena.rs:1521-1526`、`1566`、`1574`)。`Workspace` 文脈に
  しか無いバインディング (`ctrl-~` の `NewTerminal` など、Agentium で使うもの) は `main.rs` の
  `Agentium` 文脈にも登録する。Overview のメモ欄にフォーカスがある間は、エディタが消費しないキーだけ
  が届くので、`cmd-t` / `cmd-shift-t` / `ctrl-~` を実キー入力で検証する (§5)。
- インライン編集の `menu::Confirm` / `menu::Cancel` ディスパッチャ (`9601-9624`) はサイドバー子要素から
  `AgentiumApp` のルート要素に上げる。arena の名前変更エディタは Overview の Arena 一覧に移るため、
  サイドバーの下では届かない。分岐は今の 1 か所のまま (Inbox メモ、issue 入力、task 名、arena 名)。
- Arena 一覧の行は 名前、ブランチ、フォルダ と + ボタン。+ は `switch_arena` してから `add_tab` する。
  `handle_tab_new` の端末限定は変えない。

### 3.2 To do の item モデル

```rust
pub struct TodoItem {
    pub id: u64,
    pub task_id: Option<Uuid>,          // None は task に属さない arena の To do
    pub arena_path: Option<PathBuf>,    // 作成元 arena。セッションを付けるときの既定
    pub arena_name: String,             // 旧 list から引き継ぐ。path の無い item の照合と表示に使う
    pub branch: Option<String>,         // 作成時のブランチ。表示専用
    pub kind: InboxTodoKind,            // Proposal (Pick) / UserTask (You do)
    pub title: String,
    pub detail: Option<String>,
    pub created_by: TodoCreator,        // Session(InboxAsker) | Human
    pub assigned: Vec<Assignee>,
    pub state: TodoState,               // Open | Assigned | Requested | Done
    pub request: Option<Request>,       // Requested を経た item だけ Some。人が Go で付けた条件
    pub resolution: Option<Resolution>, // Done のとき必ず Some
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip)] pub choice: Option<TodoChoice>,   // 人の下書き。今と同じく非永続
}

pub struct Assignee {
    pub asker: InboxAsker,
    pub arena_path: Option<PathBuf>,    // そのセッションのタブがあった arena。復元に使う
    pub arena_name: String,
}

pub struct Request {
    pub note: Option<String>,           // Go のときのメモ (条件)。detail は書き換えない
    pub at: DateTime<Utc>,
    pub by: TodoCreator,                // Go を送った人、または自分で作った担当 AI
}

pub struct Resolution {
    pub outcome: Outcome,               // Completed | Skipped | Dismissed
    pub note: Option<String>,           // 人の Skip 理由、`done --note`、Go のときの条件
    pub at: DateTime<Utc>,
    pub by: TodoCreator,                // 閉じた側 (担当セッションか人)
}
```

- `id` が title に代わるキー。CLI の `done` / `assign` / `take` / `update` は id で指す。UI の回答も id
  で引く。title の一意性は要求しない。
- 遷移: `Open` (担当なし) → `Assigned` (担当あり、未依頼) → `Requested` (人が Go を送った、または
  担当 AI が自分で作った) → `Done`。UserTask は人の Done で `Done` (`Completed`)。Proposal は人の
  Go で担当セッションに今と同じ `[from: ...]` 形式のメッセージを送って `Requested`、メモは送信本文に
  入れると同時に `request.note` として item に残し、Requested の行に「条件」として表示する。担当者が
  後から替わっても `detail` と `request.note` の両方が見える。item の `detail` には触らない。Skip は
  `Done` (`Skipped`、理由を `resolution.note`)。AI が自分を担当にして `add` した item は `request`
  (`by` = その AI、note 無し) 付きの `Requested` から始まる。`done --note` は `resolution.note` に残る。
  `request` と `resolution` は別の記録。Done の item は `resolution` を必ず持ち、`request` は
  Requested を経た場合だけ持つ。Open から直接 Skip した Proposal や、Open から Done にした UserTask は
  `request = None` のまま閉じる (今の直接 Skip、`inbox_todo_view.rs:754-756` と同じ)。
- `Done` は消さず `resolution` 付きで残す。Overview の「Processed items」節 (セッション内だけ) は
  廃止し、Done 履歴は item の一覧の下に畳んで出す。読み込み時に `resolution.at` が 30 日より古い
  item を落とす。
- 権限: `done` と `update` は `assigned` に呼び出し元の asker が含まれるときだけ。`assign` / `take` /
  `unassign` は誰でも。UI (人) は全部できる。
- 作成者: `tab_for_ancestors` が通り `inbox_asker` が `Some` なら `Session`、`None` (素の shell、Codex
  端末タブ) なら `Human`。Agentium の端末以外からの呼び出しは今までどおり拒否する。AI の `add` は
  既定で自分を担当にして `Requested`、`--unassigned` で `Open`。Human の `add` は `Open`。
- 担当の復元: `Assignee.arena_path` は `assign` / `take` の時点で、そのセッションのタブがある arena の
  `working_directory` を書く。復元は担当者ごとに今の `todo_restore_plan` と同じ規則
  (`ResumeClaude` は session_id と `Assignee.arena_path`、`OpenArena` は `Assignee.arena_path`、
  path が無ければ `arena_name` で一意に引けた arena) を使い、item の `arena_path` は使わない。
- task の決定 (作成時): 呼び出し元 arena を含む task。2 つなら `selected_task` を優先、なければ
  `tasks_containing_worktree` の先頭。task が無ければ `task_id = None` で `arena_path` だけ持つ。
  item は task に付いたままにし、arena が別の task に移っても動かさない。task のアーカイブで item は
  非表示にする (task 復帰で戻る)。
- scope の照合: `task_id` が `Some` の item は `task_id` の一致だけで決める。worktree を共有する別の
  task や、arena が別の task に移った後の元 task には出さない。`task_id` が `None` の item
  (task に属さない arena の To do、移行時に task が引けなかった item) だけ、`arena_path` が scope の
  path に含まれるか、`arena_path` が無く `arena_name` が scope の arena 名に一致するかで決める
  (`todo_matches_scope` の規則を `None` の場合に限って item に移す)。
- 検証: title 非空。title は 200 文字、detail と note は 2000 文字を上限にする (保存と Overview の
  行幅の都合で、transport の都合ではない)。`MAX_TODO_ITEMS` は task あたりの Open + Assigned +
  Requested の上限 (暫定 30) に付け替える。`validate_todo_items` と `parse_todo_set_request` は削除。
- 永続化: `todo.json` の形式を `{ "version": 2, "items": [...] }` にし、読み込み時に旧形式 (トップ
  レベルが list の配列) を 1 回だけ移行する。旧 list の item は `assigned = [Assignee { asker,
  arena_path: list.arena_path, arena_name: list.arena_name }]` (asker が無い list は `assigned` 空)、
  `created_by = Session(asker)` か `Human`、`arena_path` / `arena_name` / `branch` は list から、
  `task_id` は `arena_path` を含む task から、無ければ `None`。`state` は asker があれば `Requested`、
  無ければ `Open`。移行のフィクスチャは `inbox_todo_view.rs:487` が書く実際の配列形式を使う。
- ドット: `assigned` ごとに `tab_for_asker` で live のタブを引き、Claude セッション状態 / Codex の
  タイトル状態からサイドバーと同じ色で出す。タブが無い担当者は灰色で「閉じている」とし、クリックで
  上の復元規則を使う。
- `InboxTodoView` は list 単位から item 単位 (Go / Skip / Done、メモ、送信) に変える。
  `overview_todo_ids` / `find_item_location` を使う To do 側の経路は上の scope 照合と `Assignee` に
  置き換える。Inbox レーンの To do 行 (`render_inbox_todo`) は task 単位に集約する。

### 3.3 CLI

```text
agentium todo add --proposal "TITLE :: DETAIL" [--unassigned]
agentium todo add --user-task "TITLE :: DETAIL"
agentium todo take <id>
agentium todo assign <id> --to self | --to "<tab title>" [--arena <path>] | --unassign
agentium todo done <id> [--note "..."]
agentium todo update <id> [--title ..] [--detail ..]
agentium todo list [--all]
agentium todo ask ...            # 変更なし
```

- transport は今の CLI stream socket (`cli_request`、`agentium-cli.sock`) のまま。要求は
  `{"type": "todo_add" | "todo_take" | "todo_assign" | "todo_done" | "todo_update" | "todo_list", ...}`、
  応答は id や一覧を JSON で返し、本体のエラーはそのまま CLI の exit 1 と stderr になる。呼び出し元の
  ancestry は `UnixStream` の peer から今までどおり取る。datagram は使わない。
- `--to "<tab title>"` は `unique_item_by_title` と `ArenaSelector` (`--arena <path>`) を使う。
  指したタブの `inbox_asker` が `None` (素の端末) なら拒否する。`Assignee.arena_path` はそのタブの
  arena から埋める。
- `list` は呼び出し元 arena を含む task の Open / Assigned / Requested を id、kind、state、担当者、
  arena 付きで返す。`--all` で Done も返す。質問は今までどおり含める。
- 書き込みは全部アプリ (単一ライター)。CLI は `todo.json` に書かない。
- `todo set` / `todo clear` と `TodoSetReply` / `TodoListReply.own/others` / `todo_set_request` /
  `parse_todo_set_request` / `dropped_todo_titles` / `todo_list_index` を削除する。
- 同じコミットで `skills/agentium-inbox/SKILL.md` (`todo set` の章) と README (322-328 行、131 行の
  コーディネーター規則) を書き換える。symlink で即時に効くので、CLI 削除と文書の書き換えを分けない。
- グローバル CLAUDE.md の「`agentium todo set` に渡す」規則は、ユーザーが `todo add` に直す
  (このリポジトリの外)。

### 3.4 Overview の To do 一覧、質問、エージェントカード

- 並び: Proposal (Pick) を上、UserTask (You do) を下。それぞれ `updated_at` 降順。Done は下に畳む。
- 各 item: 番号、title、detail、Requested 以降は `request.note` を「条件」として detail の下に、
  担当セッション行 (ドット、アイコン、タブ名、クリックで `navigate_to_item`)、担当なしは
  「Unassigned」と担当ボタン。担当ボタンは task の Live arena の
  セッション一覧から選ぶ。セッションを新規に作る場合は arena を item の `arena_path` に既定し、無くて
  task に Live arena が 2 つあるときだけ 2 択を 1 回出して item の `arena_path` に記憶する。
- 質問は To do 行とは独立に、task の arena に属するセッションごとの「質問」節として出す
  (`question_in_scope` の規則はそのまま)。To do を持たないセッションの `todo ask` も今までどおり
  Overview で答えられる。閉じた持ち主の fallback (`7643-7650`) も残す。
- 担当セッション行の下に、今のエージェントカードの request / response (`render_task_overview` の
  item ごとの描画、`7449-7475`) を畳んで置く。どの item にも付いていないセッションの request /
  response は、上の「質問」節と同じセッション単位の節に出す (消さない)。arena 単位のエージェント
  カード節は削除する。
- コーディネーター (`render_coordinator`) は Arena 一覧の行の中の切り替えに縮め、要約は担当セッション行
  の request / response に出す。

### 3.5 変更しないもの

- `Arena` と `PaneGroup` の関係、`activate_context`、`handle_tab_new` の端末限定。
- `todo ask`、`inbox_questions` の仕組み、許可 (permission) レーン、Completed / Failed レーン。
- CLI stream socket と datagram socket の役割分担。
- `board.json`、`task_coordinators.json`、`codex_acp_tabs.json` の形式。

## 4. 実装ステップ

各段は Codex のプランレビューとコードレビューを通してから次へ進む。

1. Overview カラムと task 1 行化 (モデル変更なし)。`selected_task`、`last_active_arena`、
   `Item` 実装と `open_task_overview` の削除、`ActivateTask`、`Agentium` ルートへのキー入力委譲と
   `Workspace` 文脈バインディングの複製、`menu::Confirm` / `Cancel` ディスパッチャのルートへの移動、
   Repositories タブの削除と Arena 一覧への移設、閉じた worktree 行の Overview への移設。
2. item モデルと新 CLI と文書を 1 段で。`TodoItem` / `Assignee` / `Resolution`、`todo.json` v2 と
   移行、`InboxTodoView` の item 化、Overview の To do 一覧 (担当者、Unassigned、ドット、Done 履歴)、
   `add` / `take` / `assign` / `done` / `update` / `list`、`set` / `clear` の削除、SKILL.md と README。
   旧所有者モデル (live タブ優先の `todo_list_index`、`agentium.rs:3227` / `3290` / `3329` が依存) と
   item モデルは両立しないので、1 回の atomic なマージにする。レビューのために diff を積んで分ける
   のはよいが、中間状態をマージしない。
3. Arena 一覧の + と 2 択、エージェントカードの担当セッション行への移動、質問節の独立化 (§3.4)、
   コーディネーターの規則。

## 5. テスト計画

- scope 照合の単体テスト: `task_id` が `Some` なら task の一致だけで決まる (worktree を共有する別 task
  に出ない、arena が別 task に移っても元 task に残る、task アーカイブで非表示)。`task_id` が `None`
  のときだけ arena_path / arena_name の fallback が効く。
- `request`: Go のメモが `request.note` に残り、担当者を替えた後も Requested の行に出る。
  Requested → Done の item は `request` と `resolution` の両方を持つ。Open から直接 Skip / Done した
  item は `resolution` だけを持ち `request = None` で、拒否も捏造もされない。
- 移行: `inbox_todo_view.rs:487` の配列形式の `todo.json` を読み、`assigned` (asker と arena_path と
  arena_name)、`created_by`、`state`、`task_id`、`branch` が期待どおりになること。asker 無しの list が
  `Open` / `Human` / `assigned` 空になること。path の無い旧 list が `arena_name` で scope に入ること。
- 権限: 担当外の asker からの `done` / `update` が拒否され、`assign` / `take` は通ること。
- 遷移と resolution: Proposal の Go で `Requested` と送信本文、Skip で `Done` / `Skipped` / note。
  UserTask の Done で `Completed`。`done --note` が残ること。AI の `add` が `Requested`、
  `--unassigned` が `Open`。
- 復元: arena A で作った item を arena B のセッションに assign し B を閉じたとき、担当者行の復元が
  B の path で `ResumeClaude` / `OpenArena` を返すこと。
- task 決定: arena が 2 つの task に属すとき `selected_task` 優先。task 無しで `task_id = None`。
- 上限: task あたりの未完了上限超過で `add` が拒否される。title / detail / note の長さ上限。
- GPUI テスト: タスク行クリックで `selected_task` と active arena が変わる。`switch_arena` で
  `selected_task` が追従する。Overview のメモ欄にフォーカスがある状態で `cmd-t` / `cmd-shift-t` /
  `ctrl-~` を実キー入力で送り、メニューと新規端末が active arena に出ること。Arena 一覧の名前変更
  エディタで Enter / Escape が確定 / 取り消しになること。
- 質問: To do を持たないセッションの `todo ask` が Overview に出て答えられること。サイドバーの
  To do 数に未回答の質問が含まれること。
- CLI の引数解析 (`main.rs` の既存テストの形): `add` / `assign --to` / `done --note` の組み合わせと、
  stream 応答の id / 一覧の往復。

## 6. リスクと未決事項

- symlink のスキル文書: 段 2 のコミット直後から稼働中のエージェントが `todo set` を失う。コミット前に
  動作中のタブが無いことを確認する。
- 段 2 は 1 段としては大きい (モデル、移行、UI、CLI、文書)。レビューは積んだ diff (モデル + 移行 +
  UI、CLI + 文書) で分けてよいが、マージは 1 回の atomic なものに限る。旧 CLI が旧モデルに依存する
  ため、片方だけがマージされた中間状態は動かない。
- `cmd-1`〜`cmd-9` の意味が arena から task に変わる。慣れの問題で、Unassigned arena には番号を
  振らない。
- 権限は作業規約 (決定 2)。将来境界にするなら、`assign` を人と指定セッション (コーディネーター) に
  限る段を別途置く。
- Overview カラムの幅と折りたたみ状態の永続化は今回やらない (再起動で既定に戻る)。
- 2 つの task が同じ arena を共有する場合、To do の task 決定が `selected_task` に依存する。
  CLI からは選択中の task が見えないため、`--task <TASK>` を `add` に足すかは段 2 のレビューで決める。
- `Workspace` 文脈のバインディングを `Agentium` 文脈にも複製すると、同じキーが arena 内でも 2 段で
  マッチする。GPUI は深い文脈を優先するので arena 内の挙動は変わらない想定だが、段 1 の実キー入力
  テストで確認する。

## 7. 次段 (今回は作らない)

- 自動割り当て、優先度、キュー。
- Done 履歴を ADR のふりかえりに使う経路 (2026-10-15 のメモ)。
- Overview カラムの幅と状態の永続化。
