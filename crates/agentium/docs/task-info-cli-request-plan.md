# `agentium task info` / `task list` を本体経由にする実装プラン

Rev 4 (2026-09-18)

改訂履歴:

- Rev 4: 実装レビューで判明した計画側の不足 2 件を反映。(1) `--update` の反映段 (`this.update` 内) でも
  filesystem を呼ばない。`apply_pr_fetch_result` の末尾と `save_pr_session_mapping` は arena の
  `working_directory` を foreground で `canonicalize` していたので、canonical path を引数で受ける
  `_at` 変種を切り出し、`task_info` は background で解決した canonical path を渡す (§3.5 / §3.7)。
  (2) pr.json の書き出し (`_pr_session_db_write_task`) も置き換え方式で同じ競合があるので、
  `pr_cache` / board と同じく前の Task に連結する (§3.7)。
- Rev 3: Codex 再レビューの must-fix 3 件を反映。(1) `failures` の対象に Backlog 課題 ID 解決の
  `BacklogIssueIdError::Other` を加え、`NoSuchIssue` は負のキャッシュへの正常なスキップとして除外する。
  失敗のある worktree でも ID キャッシュ (`resolved_backlog_ids` / `failed_backlog_keys`) は取り込む
  (§3.6 / §3.5)。(2) provider 不在で取得対象から外した課題は、外す時点で `warnings` に入れる (§3.5)。
  (3) `Live` の task ID 集約に `pr_fetch_context(entity_id)` (`tasks_containing_arena` が全 task の
  worktree を foreground で canonicalize する) を使わず、`board_cache.task_worktrees` の行から同じ
  `arena_index` を持つ task を集める (§3.6)。
- Rev 2: Codex レビューの must-fix 5 件を反映。(1) `fetch_pr_list` は `gh` / `bee` の失敗や provider 不在を
  `Ok(空)` で返すので、`Err` だけを見てもキャッシュを消してしまう。`PrFetchOutcome.failures` を足し、
  失敗のある worktree は反映しない (§3.6)。(2) worktree を複数 task が共有していると、対象 task だけの
  `PrFetchContext` で取得した結果が他 task の手動 PR を消す。worktree ごとに共有する全 task から入力を
  集める (§3.6)。(3) datagram と stream の順序は保証できないので、目的を「ディスク書き出し待ちの競合の
  除去」に限定し、直後反映の保証は §8 の次段に送る (§1 / §5 / §6)。(4) 課題取得の失敗が `warnings` に
  入らなかったので、`apply_issue_metadata` が失敗一覧を返す (§3.8)。(5) `_pr_cache_write_task` の置き換え
  方式では同じ `.json.tmp` に並行して書けるので、書き出しを前の Task に連結して直列化し、`--update` の
  複数 worktree 反映は 1 回だけ永続化する (§3.7)。
- Rev 1: 初版。

## 1. 目的とスコープ

`agentium task info [--task <TASK>] [--update]` と `agentium task list` を、`tab self` / `tab list` /
`tab send-message` が使っている双方向の CLI socket (`agentium-cli.sock`) 経由で本体に処理させ、本体が
メモリ上に持つ board と PR の値を返すようにする。`--update` の取得 (`gh` / `bee`) は本体が行い、結果は
ポーリングと同じ経路で本体の状態に反映してから返す。

得たいもの:

- `task add-issue X && task info` で古い board を読む主因を除く。現状は本体が board.json を background
  タスクで書き、CLI がそのファイルを読むので、書き出し前に読むと古い board を表示する。メモリ参照に
  すればこの書き出し待ちの競合はなくなる。ただし datagram (変更系) と stream (`task info`) は別スレッド
  から同じチャネルに入るため、直後に必ず反映されている保証は今回は付かない (§6、§8)。
- `--update` の取得ロジックが CLI 側に重複している状態をなくす。現 CLI 側の取得は `known_issue_ids` と
  `failed_issue_keys` が空で、Backlog 課題 ID の解決を毎回やり直し、結果は表示にだけ使って捨てる。
- `--update` の結果がサイドバーにも同時に反映される。
- Claude Code サンドボックスから `--update` するのに `gh` / `bee` の外向き通信許可が要らなくなる。
  `agentium-cli.sock` の `allowUnixSockets` は `tab` 系で既に必要。
- 副産物として `task info --json` (前回の計画でスコープ外にしたもの) が reply をそのまま出すだけで足りる。

スコープ外: `task new` / `add-issue` / `add-arena` / `add-pr` / `done` の datagram 経路の変更 (§8 次段)、
描画の変更。

## 2. 確認済みの前提

- IPC は 2 本ある。`agentium.sock` は `UnixDatagram` の片方向 (macOS の上限 2048 byte、`task` 変更系は
  1900 byte で自己制限し `New` を分割送信する; main.rs:1307 `dispatch_task_commands`)。
  `agentium-cli.sock` は `UnixStream` の request/reply で、JSON を 1 往復する (main.rs:594 `cli_request`)。
- CLI socket の本体側は、接続ごとにスレッドを起こして request を読み、`IpcMessage::CliRequest { request,
  reply: std::sync::mpsc::Sender<Value> }` を foreground のチャネルに送り、`recv_timeout(10s)` で返答を
  待つ (main.rs:1436 `start_cli_listener`、main.rs:1478 `cli_connection_reply`)。
- foreground の消費ループ (main.rs:2510) は `window_handle.update(cx, |app, _window, cx|
  handle_cli_request(app, &request, cx))` を呼び、返った `serde_json::Value` を即 `reply.send` する。
  `handle_cli_request` (main.rs:1497) は同期関数で、`tab_self` / `tab_list` / `tab_send_message` の 3 種を
  分岐する。消費ループの `cx` は `AsyncApp` で、`cx.spawn(...)` と `Task::detach` が使える。
- `cli_request` は request に `ancestor_pids` と canonical 化した `cwd` を付ける。接続失敗は種類を問わず
  `anyhow` エラーにする。
- 現 `run_task_info` (main.rs:1081) は `board::load_board()` と `agentium::load_pr_cache()` をディスクから
  読み、`--update` 時は `probe_cli_available("gh"/"bee")`、`GH_PROMPT_DISABLED=1`、`smol::block_on` で
  `fetch_issue_metadata_for` と `fetch_pr_list` を CLI プロセス内で呼ぶ。git (`rev-parse`、`remote get-url`)
  も CLI 側で同期実行する。
- 本体は `board: board::Board` (agentium.rs:339) と `pr_cache: HashMap<String, Vec<PrInfo>>`
  (agentium.rs:337、canonical path キー) をメモリに持つ。`pr_cache` は `apply_pr_fetch_result`
  (agentium.rs:923) の中で `pr_info` と同じティックに更新され、変化があれば `write_pr_cache` を
  background でスケジュールする。live な arena も閉じた arena も同じマップに載る。
- `BoardTask` (board.rs:34)、`Board` (board.rs:16)、`PrInfo` (agentium.rs:233) は `Serialize` /
  `Deserialize` を derive 済み。`IssueMetadata` (agentium.rs:4214) は derive なし。
- `pr_fetch_context(entity_id)` (agentium.rs:858) は arena の `EntityId` から `tasks_containing_arena` で
  task 群を求め、Backlog キー (bee あり時のみ) と手動 PR を集めて `PrFetchContext` を作る。
  `fetch_pr_list(working_dir: &Path, context)` (agentium.rs:3654) 自体はパスを受け取るので、閉じた
  worktree にも使える。`PrFetchContext` は `Clone` を derive していない。
- `fetch_pr_for_arena` (agentium.rs:899) は 1 回限りの取得を `cx.spawn` して `apply_pr_fetch_result` に
  渡す。`start_pr_polling` (agentium.rs:808) は 60 秒周期で、取得が 5 秒を超えると
  `pr_polling_timed_out = true` にして polling を止める。
- `fetch_issue_metadata(refresh_all)` (agentium.rs:2472) は取得結果の board への書き戻し (title / state /
  url の差分適用、`backlog_issue_ids` 更新、`persist_board`) を `this.update` 内のクロージャに直書き
  している。
- `handle_task_command` (agentium.rs:1944) は `board.apply` の失敗を `log::warn!` するだけで、datagram
  送信側の CLI は既に成功メッセージを出している。
- `resolve_target_task(board, selector)` (main.rs:906) は `std::env::current_dir()` を直接読み、
  `Board::resolve_task` と `Board::tasks_containing_path` (board.rs:205, 253) に委ねる。
- `board_cache.task_worktrees: HashMap<Uuid, Vec<WorktreeRow>>` (agentium.rs:283) は board か arena が
  変わるたびに `rebuild_board_cache` (agentium.rs:2025) で作り直され、task ごとに `task.worktrees` と
  同じ順で `Live { arena_index } | Closed { path } | Missing { path }` を 1 行ずつ持つ。canonicalize と
  `exists` はここで済んでいる。archived な task は載らないが、`resolve_task` も active な task しか
  返さない。
- GPUI の `Task<T>` (scheduler/src/executor.rs:380) には `map` 相当がない。`ready` / `detach` /
  `fallible` のみ。
- `fetch_pr_list` は取得失敗の多くを `Ok` で返す。worktree の remote 種別に対する provider が使えない
  とき `Ok(PrFetchOutcome::default())` (agentium.rs:3675)、`gh pr list` 失敗は warn して `gh pr view` へ
  進み、それも失敗なら `Ok(default)` (3750-3773)、`bee pr list` 失敗は warn して `Ok(outcome)` (3963)。
  `Err` になるのは git 失敗と JSON パース失敗など。`apply_pr_fetch_result` は `Ok(空)` で
  `pr_info` / `pr_cache` のエントリを消すので、認証切れの polling は今でもサイドバーの PR を消す。
- `write_pr_cache` (agentium.rs:3415) と `write_board` (board.rs:347) は固定の `<name>.json.tmp` に書いて
  rename する。呼び出し側は `_pr_cache_write_task` / `_board_write_task` を `Some(cx.background_spawn(..))`
  で置き換える。前の Task を drop しても、既に background スレッドで poll 中の同期 write は止まらない
  ので、2 つの書き出しが同じ tmp に並行して書ける。
- worktree は複数の task が共有できる。`add-arena` が拒むのは同じ task 内の重複だけで、
  `Board::tasks_containing_worktree` は `Vec` を返す。`pr_fetch_context(entity_id)` はその arena を含む
  全 task から Backlog キーと手動 PR を集める。
- `tasks_containing_arena` (agentium.rs:2285) は `arena_canonical_path` に加えて全 active task の全
  worktree を `std::fs::canonicalize` する。foreground で呼ばれる。
- Backlog 課題 ID の解決 (agentium.rs:3815 `resolve_backlog_issue_id`) は `BacklogIssueIdError::
  NoSuchIssue` と `Other` を区別する。`NoSuchIssue` は `outcome.failed_backlog_keys` に入り (負のキャッシュ、
  誤検出したブランチ由来キーの正常なスキップ)、`Other` (認証・通信エラー) は warn だけで何も残らない。
  ID が 1 つも解決できなければ `bee pr list` に到達せず `Ok(空)` を返す (3903-3941)。
- 既存テストは board.rs に 11、agentium.rs に 7 (純粋関数のみ)、main.rs に 3。`AgentiumApp` を
  組み立てるテストはない。

## 3. 設計

### 3.1 プロトコル

request は `cli_request` が `ancestor_pids` と `cwd` を付けてから送る。

```json
{ "type": "task_info", "task": "<selector or null>", "update": false }
{ "type": "task_list" }
```

reply:

```json
{ "ok": true, "index": 2, "task": { ...BoardTask }, "prs": { "/abs/worktree": [ ...PrInfo ] }, "warnings": [] }
{ "ok": true, "tasks": [ ...BoardTask ] }
{ "ok": false, "error": "..." }
```

- `index` は `task list` の 1 始まり番号。archived なら `null`。
- `prs` はその task の worktree の分だけ。キーは `pr_cache` と同じ canonical path 文字列。
- `warnings` は `--update` で一部の取得に失敗したときの文言。CLI は stderr に出す。
- stream なのでサイズ上限はなく、分割は要らない。

### 3.2 非同期返答

`handle_cli_request` の返り値を `serde_json::Value` から `Task<serde_json::Value>` にする。既存 3 種は
`Task::ready(value)` で包む。消費ループは

```rust
IpcMessage::CliRequest { request, reply } => {
    let task = window_handle
        .update(cx, |app, _window, cx| handle_cli_request(app, &request, cx))
        .unwrap_or_else(|error| Task::ready(json!({ "ok": false, "error": format!("{error:#}") })));
    cx.spawn(async move |_cx| {
        if reply.send(task.await).is_err() {
            log::warn!("cli client disconnected before the reply");
        }
    })
    .detach();
}
```

とし、ループ自身は待たない (他の IPC メッセージを塞がない)。spawn した Task を detach せずに drop すると
future が止まり `reply` の Sender も drop されるため、接続スレッドの `recv_timeout` が即 `Disconnected` を
返して CLI に「request failed」が出る。`detach()` は必須。

`Task<T>` に `map` はないので、`task_info` の `Task<anyhow::Result<TaskInfoReply>>` を `Task<Value>` に
変えるのは `handle_cli_request` 側でもう 1 つ `cx.spawn(async move |_this, _cx| reply_json(task.await
.map(|reply| json!(...))))` を書く。`--update` では spawn が 2 段になるが、外側は結果の畳み込みだけで
待ち時間は増えない。`update == false` は内側が `Task::ready` なので、外側の spawn だけが動く。

### 3.3 タイムアウト

`cli_connection_reply` の `recv_timeout(10s)` を定数 `CLI_REPLY_TIMEOUT = 120s` に引き上げる。複数
worktree の `gh pr list` + `bee pr list` + 課題取得を 10 秒では待てない。このタイムアウトの役目は
foreground が返答しない場合に接続スレッドを漏らさないことで、foreground が止まっているなら本体全体が
止まっているので、同期系の request も 120 秒で問題ない。

超過時は接続スレッドがエラーを返して閉じる。本体側の取得は続き、完了時の `reply.send` が `Err` になって
warn ログが出るが、結果は本体の状態には反映される。CLI 側の read にタイムアウトは付けない (現状維持;
本体が必ず何かを返して閉じる)。

代替: request に `timeout_secs` を載せて本体側で clamp する。request 種別ごとに変える必要が出たときに
採る。

### 3.4 task 解決を board.rs へ

本体側では request の `cwd` を使うので、`resolve_target_task` の中身を

```rust
impl Board {
    pub fn resolve_target(&self, selector: Option<&str>, cwd: Option<&Path>) -> anyhow::Result<Uuid>
}
```

として board.rs に移す。`selector` があれば `resolve_task`、なければ `cwd` (canonical 済み) で
`tasks_containing_path` を引き、0 件 / 複数件のエラー文言は現状のまま。`format_active_tasks` は既存の
`format_candidates` (board.rs:321) に合流させる。main.rs の `add-issue` / `add-arena` / `add-pr` は
`std::env::current_dir()` を canonical 化して渡す薄いラッパ経由でこれを呼ぶ。

### 3.5 本体側 `task_info` (agentium.rs)

```rust
#[derive(serde::Serialize)]
pub struct TaskInfoReply {
    pub index: Option<usize>,
    pub task: board::BoardTask,
    pub prs: HashMap<String, Vec<PrInfo>>,
    pub warnings: Vec<String>,
}

impl AgentiumApp {
    pub fn task_info(
        &mut self,
        selector: Option<&str>,
        cwd: Option<&Path>,
        update: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<TaskInfoReply>>;

    pub fn task_list(&self) -> Vec<board::BoardTask>;
}
```

`update == false`: `self.board.resolve_target` で task を求め、`self.board` と `self.pr_cache` から
その場で組んで `Task::ready(Ok(reply))`。`pr_cache` は live / closed 両方の最新値を持つ唯一のマップなので
`pr_info` (EntityId キー) を経由しない。

`update == true`:

1. foreground で入力をスナップショットする。worktree は `board_cache.task_worktrees[task_id]` の行を
   `task.worktrees` と zip して分類する: `Missing` は取得対象から外す、`Live { arena_index }` は
   `self.arenas[arena_index].entity_id()` を控える、`Closed` はパスだけ控える。foreground で
   canonicalize や `exists` はしない。あわせて worktree ごとの `PrFetchContext` (§3.6) と、課題の
   `IssueRef` 一覧を作る。課題は `fetch_issue_metadata` と同じ絞り込み (GitHub は `gh_available`、
   Backlog は `bee_available`) をかけるが、外した課題はその場で `warnings` に「bee unavailable; cached
   issue PROJ-1」の形で 1 件ずつ入れる。§3.8 の `apply_issue_metadata` には届かないし、worktree が
   GitHub なら PR 側の provider 不在 failure も立たないので、ここで入れないと未更新の課題が黙って
   表示される (現 CLI は全課題を取得して bee がなければ stderr に失敗を出す)。`gh_available` と
   `bee_available` が両方偽なら取得は行わず、上記の課題ごとの warning と「gh/bee not available; showing
   cached values」を入れて `update == false` と同じ reply を返す。
2. `cx.spawn(async move |this, cx| ...)` の中で background executor に、worktree ごとの `fetch_pr_list` の
   `join_all` と課題ごとの `fetch_issue_metadata_for` の `join_all` を投げ、両方を待つ。worktree ごとの
   canonical path (`pr_cache` と pr.json のキー) もこの background 側で `std::fs::canonicalize` して
   結果と一緒に foreground へ返す。反映段で filesystem を呼ばないための前準備 (§3.7)。
3. `this.update(cx, |this, cx| ...)` で反映する。
   - worktree ごと: `Err`、または `Ok(outcome)` でも `outcome.failures` (§3.6) が空でなければ、その
     worktree の `pr_info` / `pr_cache` には触らず `warnings` に積む。現 CLI の「失敗時はキャッシュ値に
     戻る」と同じ結果になる。ただし `Ok(outcome)` なら `resolved_backlog_ids` と `failed_backlog_keys`
     は失敗の有無にかかわらず `backlog_issue_ids` / `failed_backlog_issue_keys` に取り込む (ID は不変の
     事実で、次の取得を軽くする)。失敗なしの `Ok(outcome)` は、`Live` なら `apply_pr_fetch_result(entity_id, Ok(outcome), cx)`
     (CI の連鎖取得、pr.json を含む既存経路; `pr_cache` の永続化は §3.7 のとおり分離する)、`Closed` なら
     §3.7 の `apply_detached_pr_fetch(path, outcome, cx)`。arena が取得中に閉じられて `entity_id` が
     見つからない場合は `Closed` 扱いに落とす。
   - 全 worktree を反映した後に `persist_pr_cache(cx)` を 1 回だけ呼ぶ (§3.7)。
   - 課題: §3.8 の `apply_issue_metadata(results, cx)` が返す失敗一覧を `warnings` に足す。
   - 反映後の `self.board` と `self.pr_cache` から reply を組む。
4. `start_pr_polling` の「5 秒超で polling 停止」規則を踏まないため、polling ループには流さない。
   `_issue_fetch_task` / `_pr_poll_task` にも格納しない。返り値の `Task` は消費ループが所有する。

### 3.6 `PrFetchContext` を worktree 起点で作り、取得失敗を判別する

`pr_fetch_context(entity_id)` の後半 (task 群からキーと手動 PR を集める部分) を
`pr_fetch_context_for_tasks(&self, task_ids: &[Uuid]) -> PrFetchContext` に切り出し、arena 版は
`tasks_containing_arena` の結果でこれを呼ぶ。

`task_info` は対象 task の ID ではなく、**worktree ごとに、その worktree を共有する全 active task** の
ID で呼ぶ。worktree は task A と task B で共有でき、B だけが別ブランチの PR を手動リンクしている場合、
A の ID だけで取得すると B の手動 PR が結果に含まれず、共有の `pr_info` / `pr_cache` を A 分で上書きして
B のリンクが消える。task ID の集約はどちらもファイルシステムに触らない形で行う。`Live { arena_index }`
は `board_cache.task_worktrees` を走査し、同じ `arena_index` の `Live` 行を持つ task の ID を集める。
既存の `pr_fetch_context(entity_id)` は使わない: その中の `tasks_containing_arena` が全 active task の
全 worktree を foreground で `canonicalize` するので、worktree ごとに繰り返すと低速なマウントで UI と
IPC が止まる。`Closed { path }` は `self.board.active_tasks().filter(|task|
task.worktrees.contains(path))` の ID を使う (board の worktree パスは `add-arena` 時に canonical 化済み
なので直接比較できる)。集めた ID を `pr_fetch_context_for_tasks` に渡す。

集約部分は `fn task_fetch_inputs(tasks: &[&BoardTask], bee_available: bool) -> (Vec<String>,
Vec<board::PrRef>)` の純粋関数にしてテストする。

取得失敗の判別: `PrFetchOutcome` に `pub failures: Vec<String>` を足す。`fetch_pr_list` とその下位
(`fetch_github_pr_list`、`fetch_backlog_pr_list`、手動 PR の取得) で「取れなかった」を意味する箇所は、
warn と同じ文言を `failures` にも push する。対象は次のとおり。

- provider 不在 (`!provider_available` で `Ok(default)` を返す箇所)
- `gh pr list` の失敗、`gh pr view` の失敗 (stderr が「no pull requests found」のものは正常な 0 件なので
  除く)
- `bee pr list` の失敗
- 手動 PR 1 件の取得失敗
- Backlog 課題 ID 解決の `BacklogIssueIdError::Other` (認証・通信エラー)。これを入れないと、ID が全滅
  したとき `bee pr list` に到達せず `Ok(空)` で gate を通過し、閉じた worktree のキャッシュを消す。

`BacklogIssueIdError::NoSuchIssue` は対象外。存在しないキー (ブランチ名から誤検出した `FIX-123` など)
の正常なスキップで、`failed_backlog_keys` (負のキャッシュ) に入るのが正しい挙動であり、これを failure
にすると誤検出キー 1 件のために worktree 全体の反映が拒否され、負のキャッシュも保存されない。
つまり「既存の `log::warn!` と 1 対 1」ではなく、`NoSuchIssue` の warn だけは除く。

`Ok(空)` と「失敗したので空」を呼び出し側が区別できるようにするのが目的で、返り値の `Result` の意味は
変えない。既存の
`apply_pr_fetch_result` (polling) は `failures` を見ず、今の挙動 (空なら消す) のまま。認証切れで
サイドバーの PR が消える既存挙動を直すかは別件 (§6)。

### 3.7 live でない worktree の反映と、キャッシュ書き出しの直列化

`apply_pr_fetch_result` の末尾を 2 つに分ける。

- `fn update_pr_cache(&mut self, path: String, prs: Option<Vec<PrInfo>>) -> bool`: `pr_cache` の insert /
  remove だけを行い、変化したかを返す。ファイルには触らない。
- `fn persist_pr_cache(&mut self, cx)`: `pr_cache` のスナップショットを background で `write_pr_cache`
  する。

`apply_pr_fetch_result` は `update_pr_cache` が真なら `persist_pr_cache` を呼ぶ (挙動は今と同じ)。
`task_info` の `--update` は worktree ごとに `update_pr_cache` を呼び、最後に 1 回だけ `persist_pr_cache`
を呼ぶ。`apply_pr_fetch_result` を経由する `Live` の分も、ここでは `persist_pr_cache` を呼ばない版
(`apply_pr_fetch_result_without_persist`) に寄せて、永続化を末尾の 1 回にまとめる。

`persist_pr_cache` は Task を置き換えるのではなく前の Task に連結する:

```rust
let previous = self._pr_cache_write_task.take();
let snapshot = self.pr_cache.clone();
self._pr_cache_write_task = Some(cx.background_spawn(async move {
    if let Some(previous) = previous {
        previous.await;
    }
    write_pr_cache(&snapshot).log_err();
}));
```

前の Task を drop しないので取り消されず、書き出しはスケジュール順に直列化される。最後に呼ばれた
スナップショットが最後に書かれる。同じ `.json.tmp` に 2 つの書き出しが並行する経路は、polling と
`fetch_pr_for_arena` と `--update` のどれが重なってもなくなる。`persist_board` (agentium.rs:2018) も
同じ形に直す (`board.json.tmp` で同じ競合がある)。

`apply_detached_pr_fetch(path, outcome, cx)` は `backlog_issue_ids.extend(outcome.resolved_backlog_ids)` と
`failed_backlog_issue_keys.extend(outcome.failed_backlog_keys)` を行い、`update_pr_cache(path,
(!outcome.prs.is_empty()).then_some(outcome.prs))` を呼ぶ。`pr_info` には触らない (EntityId がない)。

反映段の filesystem 呼び出し: `apply_pr_fetch_result` の末尾は arena の `working_directory` を
foreground で `canonicalize` して `pr_cache` のキーにし、`pr_dirty` なら `save_pr_session_mapping` に
入って同じく `canonicalize` する (pr.json のキー)。polling ではこれまでどおりだが、`task_info` の
`this.update` からは呼ばない。それぞれ canonical path を引数で受ける変種
`apply_pr_fetch_result_without_persist_at(entity_id, canonical_path: Option<&str>, result, cx)` と
`save_pr_session_mapping_at(entity_id, canonical_path: &str, cx)` を切り出し、既存の引数なし版は
foreground で `canonicalize` してから `_at` を呼ぶ薄いラッパにする。`canonical_path` が `None`
(polling のラッパで arena の `working_directory` が取れない場合) のときは `pr_cache` の更新と
`save_pr_session_mapping_at` を行わない。従来の `save_pr_session_mapping` の早期 return と同じ結果。`task_info` は §3.5 の手順 2 で
background 側に解決させた canonical path を `_at` に渡す。

pr.json の書き出し (`save_pr_session_mapping` 内の `_pr_session_db_write_task`、`write_pr_session_db` は
固定の `pr.json.tmp` に write / rename) も置き換え方式なので、`persist_pr_cache` と同じ形で前の Task に
連結する。`--update` の一括反映で `pr_dirty` な Live arena が複数あると連続で呼ばれ、この経路を直接
踏む。

### 3.8 課題メタデータのマージを切り出す

`fetch_issue_metadata` の `this.update` 内クロージャを `fn apply_issue_metadata(&mut self, results:
Vec<(board::IssueRef, anyhow::Result<IssueMetadata>)>, cx) -> Vec<String>` に切り出す。返り値は
課題ごとの失敗文言 (`failed to fetch issue PROJ-1: ...`) で、`fetch_issue_metadata` はそれを `log::warn!`
し、`task_info` は `warnings` に足す。現 CLI は課題ごとの失敗を stderr に出しているので、これがないと
キャッシュ表示が最新の取得に成功したように見える。Backlog id の登録、変化時の `persist_board` と
`cx.notify` はこの関数の中で行う。board への差分適用は

```rust
impl Board {
    pub fn merge_issue_metadata(
        &mut self,
        reference: &IssueRef,
        title: Option<&str>,
        state: Option<&str>,
        url: Option<&str>,
    ) -> bool
}
```

として board.rs に置き、単体テストを書く。規則は現状どおり: title / state は `Some` かつ値が違えば
更新、url は既存が `None` のときだけ埋める。

### 3.9 CLI 側 (main.rs)

- `cli_request` の接続部分を分け、`NotFound` / `ConnectionRefused` を「本体なし」として区別できる
  `fn cli_request_if_running(request) -> anyhow::Result<Option<Value>>` を足す。`None` が本体なし。
  それ以外の接続エラー (Permission denied など) はエラーのまま返す。判定は `dispatch_task_commands` と
  同じ。
- `run_task_info`:
  - 本体あり: `task_info` request を送り、reply の `task` / `index` / `prs` / `warnings` (stderr) を使う。
  - 本体なし: `load_board()` と `load_pr_cache()` (現状) を使う。`--update` は「requires a running
    Agentium」でエラー (§6 決定 2)。
  - 整形は `print_task_info(index, &task, &prs_by_path)` に一本化する。git (`rev-parse`、`remote get-url`)
    は従来どおり CLI 側で同期実行する。本体に持ち込むと foreground で subprocess を待つことになるか、
    background に出す配管が増えるだけで利点がない。
  - 削除: `probe_cli_available`、`GH_PROMPT_DISABLED` の設定、`smol::block_on` の取得、main.rs 内の
    `PrFetchContext` 構築。main.rs から `smol` の参照が消えるか確認する。
  - `--json` を足す。本体ありは reply をそのまま、本体なしは同じ形を CLI で組んで `println!` する。
- `run_task_action` の `List`: 本体あり → `task_list` の `tasks`、なし → `load_board()`。出力は現状のまま。
- `handle_cli_request` に `task_info` / `task_list` の分岐を足す。`task_info` は `request["task"]`、
  `request["cwd"]`、`request["update"]` を渡し、`Result<TaskInfoReply>` を `{ ok, index, task, prs,
  warnings }` / `{ ok: false, error }` に畳む。

### 3.10 変更しないもの

- datagram 経路 (`task` 変更系、`tab new`、`pane split`、Claude hook、theme)。
- `tab self` / `tab list` / `tab send-message` の挙動。`Task::ready` に包むだけ。
- 描画、polling の周期と 5 秒ガード。

## 4. 実装ステップ

各ステップでビルドとテストが通る。

1. board.rs: `Board::resolve_target`、`Board::merge_issue_metadata` とテスト。main.rs の
   `resolve_target_task` を cwd を渡す薄いラッパにする。
2. agentium.rs: `pr_fetch_context_for_tasks` + `task_fetch_inputs`、`update_pr_cache` /
   `persist_pr_cache` の分離と書き出しの連結 (`persist_board` も)、`apply_issue_metadata` の切り出し、
   `PrFetchOutcome.failures` の配管。`Live` の polling 経路の見える挙動は不変 (書き出しが直列になる
   だけ)。
3. main.rs: `handle_cli_request` を `Task<Value>` 化、消費ループを spawn + detach に、
   `CLI_REPLY_TIMEOUT = 120s`。既存の `tab` 3 コマンドで回帰確認。
4. agentium.rs: `TaskInfoReply`、`task_info`、`task_list`、`apply_detached_pr_fetch`。main.rs の
   `handle_cli_request` に `task_info` / `task_list` 分岐。
5. main.rs: `cli_request_if_running`、`run_task_info` と `List` の切り替えとフォールバック、
   `print_task_info` への一本化、CLI 側取得コードの削除、`--json`。
6. README の `task info` / `task list` の記述を更新 (§3.9 の挙動、`--json`、`--update` は本体が
   取得して状態を書くこと、本体なしの `--update` はエラー)。

## 5. テスト計画

- (board.rs) `resolve_target`: selector があれば cwd を見ない。cwd が worktree のサブディレクトリでも
  親 task が返る。0 件と複数件はエラーで、文言に候補一覧が入る。cwd も selector も `None` はエラー。
- (board.rs) `merge_issue_metadata`: title の変化で `true`、同値で `false`。url は既存 `None` のときだけ
  埋まり、既存 `Some` は上書きしない。該当 reference を持つ task が複数あれば全部に適用。
- (agentium.rs) `task_fetch_inputs`: bee なしでは Backlog キーが空。同じキー / 同じ PR が複数 task に
  あっても 1 回。worktree を共有する task A / B のうち B だけが PR を手動リンクしているとき、
  両 task を渡せば B の PR が含まれる (§3.6 の回帰ケース)。
- (agentium.rs) `fetch_pr_list` の失敗判別は subprocess を伴うので単体テストにしない。`failures` の
  push 箇所は §3.6 の列挙 (既存の `log::warn!` から `NoSuchIssue` を除いたもの) と一致することを
  レビューで確認する。
- (agentium.rs) `Live` の task ID 集約 (`board_cache` の行から同じ `arena_index` を持つ task を集める
  部分) を `fn tasks_sharing_arena(task_worktrees: &HashMap<Uuid, Vec<WorktreeRow>>, arena_index) ->
  Vec<Uuid>` の純粋関数にし、共有 / 非共有 / `Closed` 混在でテストする。

手動確認:

1. `cargo test -p agentium` と `./script/clippy` が通る。
2. `agentium tab self` / `tab list` / `tab send-message --submit` が従来どおり動く。
3. 本体起動中に `agentium task add-issue PROJ-1; sleep 0.2; agentium task info` で PROJ-1 が出る
   (メモリ参照になっているため、board.json の書き出し完了を待たない)。`&&` 直結で必ず出ることは
   今回の保証に含めない (§6)。
4. `agentium task info --update` で PR が更新され、同時にサイドバーの arena 行にも出る。
   `pr_cache.json` と `board.json` が更新される。
5. 閉じた worktree (Tasks タブで Closed 表示) を含む task で `--update` すると、その worktree の PR も
   更新され `pr_cache.json` に載る。
5a. 同じ worktree を 2 つの task に `add-arena` し、片方だけに `add-pr` した状態で、もう片方を対象に
   `--update` しても、リンクした PR がサイドバーと `pr_cache.json` から消えない。
6. `gh auth logout` の状態で `--update` すると stderr に warning が出て、PR はキャッシュ値のまま
   (`pr_cache.json` のその worktree のエントリが消えない)。`bee` を PATH から外して Backlog の worktree
   を含む task で `--update` しても同様。
6a. `gh` は通るが `bee auth logout` した状態で、GitHub PR は更新され、Backlog 課題の分だけ stderr に
   warning が出る。
6b. GitHub の worktree と Backlog 課題を持つ task で、`bee` を PATH から外して `--update` すると、PR は
   更新され、課題ごとに「bee unavailable; cached issue PROJ-N」が stderr に出る (worktree 側には
   failure が立たないケース)。
6c. Backlog の worktree で、ブランチ名に存在しないキー (`FIX-123` など) と実在するキーが両方ある
   状態で `--update` すると、実在キーの PR は更新され、`FIX-123` は warning にならず 2 回目の
   `--update` では解決を試みない (`failed_backlog_issue_keys` に入る)。
6d. Backlog の worktree で `bee auth logout` して `--update` すると (ID 解決が全滅して `bee pr list` に
   到達しないケース)、stderr に warning が出て、その worktree の PR はキャッシュ値のまま
   `pr_cache.json` から消えない。
7. 本体を止めて `task info` するとディスクの値が出る。`--update` はエラー。`task list` も出る。
8. Claude Code サンドボックス内 (`allowUnixSockets` に `agentium-cli.sock` のみ) から `--update` が通る。
9. `agentium task info --json | jq .task.title` が読める。
10. worktree が 3 つ以上ある task で `--update` を連続 2 回実行しても、`pr_cache.json.tmp` が残らず、
   ログに rename 失敗が出ない。

## 6. リスクと未決事項

- 決定 1: README は `--update` を「board.json も pr_cache.json も書かない」と定義している。本体経由に
  するとポーリングと同じ経路で状態を書き、UI にも反映される。ファイルに書くプロセスは本体だけで、
  本体内の書き出しは §3.7 で直列化する (プロセスが 1 つであることと書き出しが 1 つずつであることは
  別で、後者は今の置き換え方式では成立していない)。推奨: 採用。
- 決定 2: 本体なしの `--update` (CLI プロセス内の `gh` / `bee` 取得) を廃止するか。推奨: 廃止。重複した
  取得実装が消え、`main.rs` から `smol::block_on` と `PrFetchContext` 構築がなくなる。残す場合は
  ステップ 5 の削除をせず、フォールバック側に現行コードを残す (約 60 行の維持)。
- 順序保証は今回付かない。datagram listener と stream listener は別スレッドで、それぞれが受信・パース
  して同じ `IpcMessage` チャネルに push する。`dispatch_task_commands` は `send` が返れば終わるので、
  datagram がカーネルのキューに入った後でも、datagram スレッドが前のメッセージ (Claude hook など) を
  処理中なら stream の `CliRequest` が先に foreground に届く。今回の受け入れ条件は「board.json の
  書き出し待ちの競合がなくなる」までで、`add-issue && info` の直後反映は変更系の stream 化 (§8) で
  変更コマンドの適用 ACK を CLI が待つようになったときに保証される。
- `fetch_pr_list` に内部タイムアウトはない。120 秒を超えると CLI にはエラーが返り、本体は完走して
  状態に反映する。
- `--update` 中に同じ arena の polling や `fetch_pr_for_arena` が重なると `apply_pr_fetch_result` が
  2 回走るが、メモリ上は後勝ちで整合し、ファイルは §3.7 の連結で直列に書かれる。
- polling の `apply_pr_fetch_result` は `failures` を見ないので、認証切れの polling がサイドバーの PR を
  消す既存挙動は残る。`failures` が空でなければ消さない、に変えるのは 2 行だが挙動変更なので別件と
  する。
- 消費ループで `window_handle.update` が失敗する (window が閉じている) と即エラー返答になる。現状と同じ。
- `handle_cli_request` は `handle_tab_send_message` などの `Result<()>` を JSON に畳む箇所が増えるので、
  `fn reply_json(result: anyhow::Result<Value>) -> Value` 程度の共通化に留める。

## 7. 見積り

| 範囲 | 目安 |
|---|---|
| board.rs (`resolve_target`、`merge_issue_metadata`、テスト) | 80 行 |
| agentium.rs 切り出し (§3.6 / 3.7 / 3.8、`failures` の push、書き出しの連結) | 70 行 (うち移動 30 行) |
| agentium.rs 新規 (`TaskInfoReply`、`task_info`、`task_list`、`apply_detached_pr_fetch`) | 140 行 |
| main.rs (`Task<Value>` 化、消費ループ、`cli_request_if_running`、`task_info` / `task_list` 分岐、`--json`) | +90 行 |
| main.rs 削除 (CLI 側取得、`probe_cli_available`、`resolve_target_task` 本体) | −80 行 |
| README | 15 行 |

## 8. 次段 (今回は作らない)

`task new` / `add-issue` / `add-arena` / `add-pr` / `done` を同じ stream socket に移す。
`handle_task_command` の `board.apply` 失敗が CLI に返るようになり、1900 byte の datagram 上限と
`dispatch_task_commands` の `New` 分割が消える。本体なし時のファイル直書きフォールバックは
`cli_request_if_running` の `None` 分岐でそのまま残せる。
