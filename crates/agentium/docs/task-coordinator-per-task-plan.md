# コーディネーターを arena からタスクへ移す実装プラン

Rev 4 (2026-11-12)

改訂履歴:

- Rev 4: 実装とコードレビュー (Codex 承認、2 ラウンド) で決めたこと。(1) tick は停止 (実行可能でなく
  なったタスク) と終了検知を、保存エラー・保存待ちのガードより前に行う。保存が失敗し続けても
  archive したタスクのプロセスが残らず、死んだビューを Retry が使い回さない。(2) ジョブは送信直前に
  保存が終わっていること (保存中でなく、保存エラーも無い) を確かめてから送る。保存されていない
  job id を外へ渡さないため。(3) Codex の接続が Error のときの Retry は、ビューを捨てて作り直す。
  (4) 終了検知・tick による停止・タスクの切り替えでも、ビュー内のフォーカスを外へ移す。
  (5) `agentium-inbox` スキルにコーディネーターの例外を書く。CLI のコーディネーター分岐のテストは
  `AgentiumApp` のテストハーネスが無いので実機確認 (§5) に回す。

- Rev 3: Codex 再レビューの 6 点。(1) 終了検知はイベント購読ではなく tick ごとの `kill(pid, 0)`
  (§3.3)。(2) 未完了ジョブの移譲で job id を振り直し、旧担当の遅れた callback を失効させる。(3)
  「実行可能」= タスクが存在し、archive されておらず、コーディネーターが enabled。移譲・起動・送信・
  完了受付で共通に使う (§3.4)。(4) v1 の同一 owner の統合は Source が一致するときだけ要約を優先し、
  同じ turn なら response のある方、それ以外の衝突は復元しない (§3.1)。(5) コーディネーターの
  `todo add` は常に未割り当て (§3.6)。(6) 固定ディレクトリの作成を両 provider 共通の起動前処理に (§3.2)。

- Rev 2: Codex プランレビュー (REQUEST CHANGES) の 6 点を反映。(1) Codex ACP の host コールバック
  (表示判定・通知クリック・端末ログイン) をコーディネーターのビューにもつなぐ (§3.9)。(2) Claude の終了を
  購読してビューを回収し、Retry で再起動する (§3.3)。(3) `todo list` は項目もタスクの scope にする。
  (4) `todo assign --to <title>` はタスクの Live arena から一意なタブを探す (§3.6)。(5) ジョブを
  コーディネーターごとではなく owner キーの全体表に移し、タスクをまたぐ重複と停止済みの所有者を
  なくす (§3.1 / §3.4)。(6) 停止前と終了時にセッション情報を保存し、Claude の開始は SessionStart
  フックで記録する (§3.3)。ほかに v1 の退避、arena 非依存の状態取得、権限待ちの見出し表示、
  Pane 外のメニューとフォーカスを追加。

## 1. 目的と決定事項

コーディネーター (要約役の Claude Haiku / Codex Luna セッション) は今 arena ごとにあり、Task Overview の
arena 行ごとに操作が並ぶ。これをタスクに 1 つにする。2026-11-12 にユーザーが決めたこと:

1. 表示先は「なし」。どの arena のペインにも置かない。人の操作が要るときのために、Task Overview の
   カラムに「Coordinator」タブを足し、そこでだけ表示する。
2. 実行ディレクトリは全コーディネーター共通の固定ディレクトリ `paths::data_dir()/coordinator`。
3. 2 つ以上のタスクに入っている arena のターンは、To do と同じ決まり (`todo_task_for_arena`: 選択中の
   タスクがその arena を含めばそれ、無ければ最初のタスク) で 1 つのタスクに振り分ける。
4. タスクの無い arena (Unassigned) にはコーディネーターを置かない。

## 2. 現状 (調査結果)

- 保存: `task_coordinators.json` の `Saved { version: 1, arenas: BTreeMap<PathBuf, Coordinator> }`。
  実データの 5 件はすべて `enabled: false`。
- 実行時状態: `AgentiumApp.coordinator_runtime: HashMap<PathBuf, Runtime>`、`Runtime.item: Option<EntityId>`
  は arena のペインにあるタブの id。
- 起動: `Arena::start_coordinator_claude` が arena の `working_directory` で `claude --session-id|--resume`
  を起動してアクティブペインに置く。Codex は `Arena::add_codex_acp` で arena の作業ディレクトリに作る。
- 監視: `coordinator_tick` が arena ごとに、その arena のペインのタブから Source を集めて `observe` する。
  Claude のターンはフック経由の `coordinator_claude_turn` が祖先 PID → arena → path でコーディネーターを引く。
- 表示: `render_coordinator(arena)` を Overview の arena 行 (`render_overview_arena_row`) が呼ぶ。
  要約は `coordinator_message(path, item, ...)` が arena の path でコーディネーターを引いて出す。
- arena のペインを走査して探す処理: `tab_for_ancestors` (`todo` CLI の呼び出し元特定)、
  `coordinator_provider_for_item`、`coordinator_tick`、`launch_coordinator` の既存タブ探し、
  `render_coordinator` の Retry / New session の生存確認 (`find_item_location`)。
- 起動時、enabled なコーディネーターの arena が閉じていれば `start_coordinator_poll` が開き直す。
- Claude のセッションファイルは `~/.claude/projects/<実行ディレクトリの slug>/<id>.jsonl` に保存される
  (`.worktrees/a` のコーディネーターで確認)。実行ディレクトリが変わると `--resume` の前提が崩れる。

ペインに置かないことの技術的な確認 (コード):

- `Terminal` の PTY イベントループは `Terminal::subscribe` が自分で spawn し、端末問い合わせへの応答
  (`PtyWrite`) もそこで処理する (`terminal.rs:1457`, `:1723`)。描画しなくてもプロセスは動く。
- フックのセッション登録 (`register_claude_session`) は PID だけを使う。
- 罠: `Terminal::paste` は bracketed paste の判定に `last_content.mode` を使うが、`last_content` は
  描画時の `Terminal::sync` でしか更新されない (`terminal.rs:2526`)。描画しない端末に貼ると改行が Enter
  になり、ジョブ文面 (指示 + 空行 + データ) が途中で送信される。
- `Terminal` の Drop は子プロセスを kill する (`terminal.rs:3545`)。ビューを捨てればプロセスも止まる。

## 3. 設計

### 3.1 保存 (version 2)

```rust
struct Saved {
    version: u32,                                  // 2
    tasks: BTreeMap<Uuid, Coordinator>,            // セッション設定だけ
    jobs: BTreeMap<String /* owner */, Job>,       // 全体で 1 表
}
struct Coordinator { id, enabled, provider, instructions_version,
                     claude_session, claude_started, codex_id, codex_tab }   // jobs を外す
struct Job { id, source, summary, task: Option<Uuid> /* 処理担当のタスク */ }
```

- owner (`claude:<session>` / `codex:<persistent id>`) ごとにジョブは 1 つだけ。タスクをまたいで
  同じターンが 2 回要約されることも、旧版の Source が別タスクに残ることも、構造上起きない。
- version 1 を読んだら、元ファイルを `task_coordinators.v1.json` に複製してから変換する。
  ジョブ (完了済みの要約を含む) は owner をキーに引き継ぎ、`task: None` にする (どのタスクで処理する
  かは次の `observe` で決まる)。arena ごとのセッション設定 (`claude_session` など) は捨てる。
  arena の作業ディレクトリで始めた Claude セッションは固定ディレクトリから `--resume` できないため。
  同じ owner が複数 arena にあるとき (`v1_merge` として純粋関数にする):
  - Source が一致 → 要約のある方を残す。
  - Source が違い、同じ turn で request が一致し、片方だけ response がある → response のある方を残す
    (response はターンの後で追記されるので、こちらが新しい)。
  - それ以外 → 新旧を決められないので、その owner は引き継がない (Claude の Source キャッシュに
    古い値を最新として復元しないため)。
- 変換に失敗したときや未知の version は従来どおりエラーでストアを disabled にする。

### 3.2 実行時状態とビューの持ち方

```rust
enum CoordinatorView { Claude(Entity<TerminalView>), Codex(Entity<CodexAcpView>) }
struct Runtime { view: Option<CoordinatorView>, exited: bool, launch_token, attempted,
                 configuring, configured, sent, error, waiting_since, _subscription }
coordinator_runtime: HashMap<Uuid /* task */, Runtime>
```

- ビューはどのペインにも追加しない。`Runtime.view` が唯一の強参照。
- 起動前処理 (両 provider 共通): `create_dir_all(data_dir()/coordinator)`。失敗は `Runtime.error`。
  Codex の `CodexAcpServer::connect` はディレクトリの存在を確認するので、Claude だけでは足りない。
- Claude: `project.create_terminal_with_shell(Some(data_dir()/coordinator), shell, cx)` で作り、
  `TerminalView::new(..)` で包む。`set_show_workspace_actions(false)` で
  「Close Terminal Tab」などペイン前提のメニューを出さない。生成は `Arena` から
  `task_coordinator.rs` に移す (`AgentiumApp` の `project` / `workspace` を使う)。
- Codex: `CodexAcpView::new(project, workspace, Some(data_dir()/coordinator), TITLE, saved, ..)`。
  `set_arena` は呼ばない。`suppress_completion_notifications` は今と同じ。
- 送信: Claude のビューに送る前に `terminal.sync(window, cx)` を呼ぶ (bracketed paste 判定が
  描画時にしか更新されない `last_content` に依存するため)。
- 状態: `tab_info(item, &arena, ..)` から状態判定を arena 非依存の `item_state(item)` に切り出し、
  tick とOverview の見出しはそれを使う。

### 3.3 ライフサイクル

- 起動: enabled で、そのタスクが担当の未完了ジョブがあり、まだ `attempted` でないときに起動する。
  ジョブが無ければ起動しない。Coordinator タブの「Start」でも起動する。
- `start_coordinator_poll` の「enabled なコーディネーターの arena を開き直す」処理は削除する。
- 終了検知: Claude は tick ごとに端末の `pid_getter().fallback_pid()` を `kill(pid, 0)` で確かめる
  (既存の caffeinate 監視と同じ方法。終了した子は Alacritty のイベントループが回収するので kill が
  失敗する)。`TerminalView` は `CloseTerminal` を `ItemEvent::CloseItem` に変えて転送し、端末側も
  入力前の非ゼロ終了では `CloseTerminal` を出さないので、イベント購読には頼らない。tick で見るので
  購読前の終了も拾う。終了したら `exited = true`、`view = None`、
  `error = "Coordinator exited. Retry to restart."` にする。自動では再起動しない (起動直後に落ち続けるときに 2 秒ごとに再起動しないため)。Retry は
  `Runtime` を初期化し、`claude_started` なら `--resume` で再開する。Codex はビューの状態が Error の
  とき今と同じエラーにする。
- 開始の記録: Claude は `register_claude_session` (SessionStart フック) で、session id がいずれかの
  コーディネーターの `claude_session` と一致したら `claude_started = true` を保存する。次の tick を
  待たないので、起動直後に Disable しても次回 `--session-id` を重ねて選ばない。
- 停止 (Disable、Create new session、および tick でタスクが「実行可能」でなくなったと分かったとき): ビューを捨てる前に
  `snapshot_coordinator(task)` で Codex の `saved_tab()` を `codex_tab` に書き、Claude は
  `claude_sessions` に session があれば `claude_started` を立てる。アプリ終了時も `Store::finish()` の
  前に全タスク分を snapshot する。ビューを捨てると `Terminal` の Drop が子プロセスを止める
  (PTY 解放後に猶予を置いて kill)。
- 初回起動時の固定ディレクトリの信頼確認 (Claude) は描画されないので、「60 秒 ready にならない」
  エラーで止まる。エラー文に「Coordinator タブを開いて確認」を添える。

### 3.4 Source の収集と振り分け

- 振り分け先: arena のタブのターンは `todo_task_for_arena(arena_index)` のタスク (タスクの無い arena は
  対象外)。tick は enabled なタスクごとではなく、Live arena ごとに 1 回、その振り分け先を求めて
  `observe(owner, source, route)` する。`coordinator_claude_turn` も祖先 PID → arena → 同じ規則。
- 「実行可能」(`runnable(task)`): タスクが board に存在し、archive されておらず、そのタスクの
  コーディネーターが enabled。ストアは board を知らないので、`AgentiumApp` が実行可能なタスクの集合を
  求めて渡す。移譲・起動・送信・完了受付のすべてで同じ判定を使う。
- `observe(owner, source, route, runnable)`:
  - 既存ジョブと Source が違う → 新しい id のジョブで置き換え、`task = route` (旧 id は失効し、
    `summary set` は「superseded」で拒否される。今と同じ挙動が全体で成り立つ)。
  - Source が同じで未完了、かつ `task` が None か実行可能でない → `task = route` に移し、id を
    振り直す。旧担当に送ってあった callback が後から届いても、id が違うので拒否される。
  - Source が同じで、担当タスクが実行可能 → 何もしない (選択タスクの切り替えで振り分け先が変わって
    も、処理中のジョブは元のタスクのまま)。
  - 完了済みのジョブは移さない。
- `route` が実行可能でなければ、ジョブは作るが `task = route` のまま待つ (後で Enable されれば処理
  される。その前に別の振り分け先が実行可能になれば、上の規則で移る)。
- 移譲で id が変わったジョブを送信済み (`Runtime.sent`) としていた旧担当の `Runtime` は、次の tick で
  その id が無いことを見て `sent` を外す。
- 自分自身の除外は今と同じ (`claude_session` / `codex_id` の一致)。ビューがペインに無いので
  `runtime.item` による除外は不要になる。
- 起動時の `coordinator_claude_sources` 復元は全体表から行うので、owner ごとに 1 つに定まる。
- `complete(job_id, summary, runnable)`: 全体表から id で引き、担当タスクが実行可能か確認する。
- `invalidate_coordinator_claude_turn`: 全体表から owner を消す。

### 3.5 要約の表示

- `coordinator_message(item, ..)` から `path` 引数を外し、全体表の owner のジョブで Source が一致する
  ものの要約を出す。振り分け先に関係なく、どの Overview でも出る。

### 3.6 `todo` CLI の呼び出し元

- `todo_caller` は `tab_for_ancestors` の前に、祖先 PID がいずれかのコーディネーターのビューの PID
  (Claude: 端末の `fallback_pid`、Codex: `process_id`) と一致するかを見る。一致したらそのタスクに
  帰属させる。
- `TodoCaller` は `arena: Option<Entity<Arena>>` と `task_id: Option<Uuid>` (コーディネーター用の
  上書き) を持つ。
  - `todo add`: task は上書きを使い、`branch` と `arena_path` は無し、`arena_name` は `TITLE`。
    `--unassigned` の有無にかかわらず常に未割り当てで作る (担当になると `done` / `update` の権限が
    付くため。指示文だけでは禁止にならない)。
  - `todo list`: 項目も質問も `todo_scope_for_task(task_id)` と、そのタスクの Live arena 全部を範囲にする。
  - `todo assign --to <title>` (`--arena` 無し): 呼び出し元の arena の代わりに、タスクの Live arena の
    タブから一意なタイトルを探す。0 件・複数件はエラーで、複数件なら「`--arena <path>` で絞る」と返す。
    `--arena` があれば今と同じ。
  - `todo take` / `assign --to self`: コーディネーターは担当になれない (要約役は作業しないため)。
    エラーを返す。
- 権限規則 (`done` / `update` は担当のみ) は変えない。`coordinator summary set` は呼び出し元を見ない
  ので変更なし。

### 3.7 UI

- arena 行から `render_coordinator` を外す。
- Overview の見出し (Task scope のときだけ) に 1 行:
  `Coordinator · Experimental  [Enable/Disable] [Claude · Haiku / Codex · Luna]  <状態>  N pending`
  `<状態>` は `item_state` から Running / Waiting for permission / Idle / Not started。権限待ちのときは
  警告色にし、押すと Coordinator タブを開く。エラー時は従来の Retry / Create new session を同じ場所に。
- Overview カラムの見出し直下にタブ「Overview | Coordinator」。Coordinator はコーディネーターが
  enabled のときだけ出す。選択はタスクごと (保存しない)。Coordinator タブはビューをカラムいっぱいに
  描画し、ビューが無いときは「Start」ボタンを出す。
- フォーカス: Coordinator → Overview の切り替え、Disable、Overview の折りたたみでビューが消えるとき、
  フォーカスがビュー内にあれば Overview 折りたたみと同じく active arena (無ければ root) に移す。
- `coordinator_provider_for_item` は `Runtime.view` の entity id と、従来の session / codex_id 一致で判定。
  ペインに無くなるので、セッションカード・完了通知・サイドバーのドットには出なくなる。
- `sync_codex_tabs` の「コーディネーターの Codex タブを除外」は、コーディネーターがペインに無くなる
  ので削除する。

### 3.8 文言

- `INSTRUCTIONS` の「for this Arena」を「for this task」に。`todo assign` で同名タブが複数あるときは
  `--arena` を付けるよう 1 文足す。
- README の Task Coordinator 節と Task Overview 節、`agentium-inbox` スキルの記述を合わせる。

### 3.9 Codex ACP の host コールバック

`init_acp_host` (`agentium.rs:107`) の 3 つは arena のペインから会話を探している。コーディネーターの
`CodexAcpView` の `conversation_id` も探す。

- `is_conversation_visible`: コーディネーターのビューなら、Overview がそのタスクを表示中で、Coordinator
  タブが選択中で、ビューにフォーカスがあるとき true。
- `activate_conversation` (通知クリック): そのタスクを選択し、Overview を開き、Coordinator タブに切り替え、
  ビューにフォーカスする。
- `spawn_terminal_login`: コーディネーターなら、作業ディレクトリを固定ディレクトリにして、active arena
  のペインにログイン用ターミナルを置く。active arena が無ければ「arena を開いてから再試行」のエラー。

## 4. テスト

- `Saved`: version 2 の往復、version 1 の変換 (ジョブ引き継ぎ・`task: None`・セッション設定破棄・
  同一 owner の要約優先)、未知 version はエラー。v1 の退避ファイルが作られる。
- `observe`: Source 変更で置き換え (旧 id の `complete` は失敗)、実行可能でないタスクの未完了ジョブが
  `route` に移り id が変わる (旧 id の `complete` は失敗)、両方実行可能なら移らない、完了済みは移らない。
- `complete`: 担当タスクが実行可能でない (disabled / archive / 削除) なら失敗。
- `v1_merge`: Source 一致で要約優先、同じ turn で response のある方、それ以外は引き継がない。
- コーディネーターの `todo add` は `--unassigned` 無しでも未割り当て、`take` / `assign --to self` は失敗
  (`todo_add` 内の判定を純粋関数に切り出せる範囲で)。
- 振り分け: `todo_task_for_arena` と同じ規則の純粋関数 (選択タスク優先、無ければ最初)。
- `coordinator_message`: 担当と別タスクの Overview でも要約が出る。
- ライフサイクル (`AgentiumApp` のテストハーネスが無いので、状態遷移を純粋関数に切り出せる範囲で):
  終了 → error、Retry で `attempted` が戻る、開始記録が SessionStart で立つ。

## 5. 実機で確認すること

- 描画しない Claude 端末にジョブを送り、要約が登録される (改行で途中送信されない)。
- 初回の信頼確認が Coordinator タブで答えられ、以後 `--resume` で再開できる。
- Claude を `/exit` で終了すると見出しに「exited」が出て、Retry で再開する。
- Codex ACP が git リポジトリでない固定ディレクトリで起動できる。通知クリックで Coordinator タブが開く。
- 2 タスクに入っている arena のターンが 1 回だけ要約される。選択タスクを切り替えても増えない。
- Coordinator から `todo add` / `list` / `assign --to` が使える。
- Disable / archive でプロセスが消える (`ps` で claude / codex-acp が残らない)。

## 6. リスク

- Codex ACP が git リポジトリ外を拒否する場合、固定ディレクトリで `git init` する案に切り替える。
- `TerminalView` をペインの外で描画したときに、ペイン前提の処理が残っている可能性。描画は Overview
  の中だけで、Item としての操作は使わない。
- 描画しない端末では `Terminal.events` (内部イベントのキュー) が `sync` まで溜まる。送信前に `sync` を
  呼ぶので、溜まるのは送信間の数件に留まる。
- 初回起動で Claude の SessionStart フックが信頼確認より前に届くと、tick は ready とみなして
  ジョブを貼る。貼り先が信頼確認のダイアログになる可能性があるので、実機で順序を確認する。
  先に届くなら、初回 (`claude_started` が偽) はジョブを送る前に Coordinator タブで人が確認する待ちを入れる。
- v1 の arena ごとの Claude セッション (開始済み 2 件) は引き継がない。会話の続きが要る場合は
  退避ファイルの session id で `claude --resume` を各 worktree から手で開く。
