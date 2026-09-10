# `agentium task info` / `agentium task add-pr` 実装プラン

Rev 2 (2026-09-09)

改訂履歴:

- Rev 2: 実装後のユーザー確認で、手動リンクした PR は「タスク直下の別リスト」ではなく「同じリポジトリの arena 行に、
  発見された PR と同列に出す」ものだと判明。§3.3 / §3.4 のタスク単位の取得と描画 (`task_pr_info`、
  `fetch_task_pr_metadata`、`render_task_pr_row`) を撤去し、`fetch_pr_list` が `PrFetchContext` 経由で手動 PR を
  受け取って発見分と結合する設計に置き換えた。`PrLink` は廃止して `BoardTask.prs: Vec<PrRef>` に、`add-pr` は
  タスクの worktree の origin と一致しないリポジトリの PR を拒否する。Backlog PR の B バッジは arena 行の
  全 Backlog PR (発見分を含む) に付く。`task info` の手動 PR 節は廃止し、arena 節の PR 行に `(linked)` を付ける。
- Rev 1: 初版。

## 1. 目的とスコープ

CLI に 2 つのサブコマンドを追加する。

- `agentium task info [--task <TASK>] [--update]`: カレントディレクトリから task を割り出し、task の
  arena (worktree)、PR、issue を一覧表示する。`--update` で PR と issue を取り直してから表示する。
- `agentium task add-pr <PR> [--task <TASK>]`: PR を task に手動で紐付ける。`add-issue` と同じ位置付けで、
  ブランチから発見できない PR (別リポジトリ、ブランチ削除後、他人のブランチ) を扱う。

アプリ側は、手動で紐付けた PR をタスク配下に描画する。GitHub は arena 行の既存 PR 表示 (状態アイコン、
`#番号`、CI アイコン、レビュー要素) をそのまま使い、Backlog は先頭に B バッジを付けて状態アイコンと
`#番号` だけ出す。

スコープ外: `task info --json`、手動 PR の CLI からの削除、Backlog PR から課題を自動登録すること。

## 2. 確認済みの前提

- `BoardTask` (board.rs) は id / title / issues / worktrees / archived を持ち、PR のフィールドはない。
- arena の PR は `AgentiumApp::pr_info: HashMap<EntityId, Vec<PrInfo>>` (agentium.rs:275) のメモリ上に
  しかなく、永続化されていない。`pr.json` は PR 番号と Claude セッション ID の対応で、PR 表示の
  キャッシュにはならない。
- `PrInfo` (agentium.rs:193) は `SharedString` を含む。`SharedString` は serde の Serialize / Deserialize を
  実装している (crates/gpui_shared_string/gpui_shared_string.rs:194, 203)。
- `fetch_pr_list` (agentium.rs:3142) はブランチ駆動で、番号指定の取得はない。GitHub は `gh pr list --head`、
  Backlog は `bee pr list --issue` を使う。
- `bee pr view <N> -p <project> -R <repo> --json` は `pr list` と同じ形の JSON を返す
  (number / summary / base / branch / status.name)。実リポジトリで確認済み。`--json <fields>` で絞れる。
- `fetch_ci_status` (agentium.rs:3552) は `gh pr checks <N>` を cwd のリポジトリで実行する。`--repo` は
  渡していない。
- `fetch_reviews` (agentium.rs:3482) は PR URL から API パスを組み、`working_dir` は cwd にしか使わない。
- arena 行の PR 要素は `render_arena_row` の中にインラインで組み立てられている (agentium.rs:3805 から
  3960 付近)。ツールチップは `render_pr_tooltip` として独立している。
- タスク配下は issue 行、その後に worktree 行を描く (agentium.rs:4403 から 4430)。live な worktree は
  `render_arena_row` をそのまま描くので、arena 由来の PR はタスク配下にも出る。
- issue 行の右クリックメニューは `deploy_issue_context_menu` (agentium.rs:2054) で Open in Browser と
  Remove from Task を出し、削除は `remove_issue_from_task` がアプリ内で board を直接変更する。
- `fetch_issue_metadata` (agentium.rs:2128) は provider の CLI 有無で対象を絞り、`join_all` で取得し、
  変化があれば `persist_board` する。手動 PR のメタデータ更新はこれを写す。
- `start_pr_polling` (agentium.rs:756) はアクティブ arena だけを 60 秒周期で取得する。
- `resolve_target_task` (main.rs:648) は canonical な cwd と worktree の完全一致で task を探す。
  サブディレクトリからは失敗する。
- CLI 分岐 (`Command::Task`, main.rs:1256) は `zlog::init()` (main.rs:1295) より前に return するため、
  ライブラリ側の `log::warn!` は CLI では捨てられる。`zlog::init_output_stderr()` が存在する
  (crates/zlog/src/sink.rs:51)。
- IPC は片方向の datagram で、`task_command` の中身は `serde_json::from_value::<TaskCommand>` で読む
  (main.rs:969)。`TaskCommand` に variant を足しても listener の変更は要らない。
- board.json は単一ライター。アプリ起動中に CLI が書き戻すことはできない。

## 3. 設計

### 3.1 モデル (board.rs)

`PrStatus` と `PrProvider` を agentium.rs から board.rs に移し、serde を付ける。agentium.rs 側は
`pub use board::{PrProvider, PrStatus};` で再エクスポートし、既存の参照はそのまま通す。

```rust
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PrStatus { Draft, Open, Merged, Closed, Conflicted }

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PrProvider { GitHub, Backlog }

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum PrRef {
    #[serde(rename = "github")]
    GitHub { repo: String, number: u32 },
    Backlog { web_host: String, project: String, repo: String, number: u32 },
}
```

(Rev 2) 当初あった `PrLink` (title / status のキャッシュ付き) は廃止した。手動 PR は arena の `pr_info` に
合流するので、arena 由来の PR と同じくキャッシュは `pr_cache.json` 側で持つ。

- `PrRef::number()`, `PrRef::provider()`, `PrRef::short_label()` (`{repo}#{number}`)、`PrRef::html_url()` を
  用意する。URL は GitHub が `https://github.com/{repo}/pull/{n}`、Backlog が
  `https://{web_host}/git/{project}/{repo}/pullRequests/{n}` で、どちらも参照から一意に組めるため
  `IssueLink` と違って url フィールドは持たない。
- `BoardTask` に `#[serde(default)] pub prs: Vec<PrRef>` を追加する。既存 board.json は読めるので
  `BOARD_FORMAT_VERSION` は据え置く。Rev 1 の `PrLink` 形 (title / status 付き) で書かれた既存エントリも、
  内部タグ付き enum は未知フィールドを無視するので読める (テストで確認)。
- `TaskCommand::AddPr { task_id: Uuid, pr: PrRef }` を追加し、`Board::apply` に等値で重複を弾く arm を足す。
- `parse_pr_ref(input) -> anyhow::Result<PrRef>` を追加する。受け付ける形は次の 3 つ。
  - `https://github.com/{owner}/{repo}/pull/{n}` (`?`, `#` 以降と末尾の `/files` 等は捨てる)
  - `https://{host}/git/{project}/{repo}/pullRequests/{n}`
  - `owner/repo#N`
  素の番号は cwd の origin が要るので board.rs では扱わず、CLI 側 (3.6) で解決する。
- `Board::tasks_containing_path(&self, path) -> Vec<&BoardTask>` を追加する。`path.ancestors()` を順に
  `tasks_containing_worktree` に通し、最初に見つかった階層の結果を返す。`resolve_target_task` を
  これに切り替えると add-issue / add-pr / info がサブディレクトリから使えるようになる。
  `add-arena` は例外で、祖先照合で親 worktree の task が解決された後にサブディレクトリを新しい worktree
  として登録してしまう。`AddArena` では task 解決後、canonical なパスが解決した task のいずれかの
  worktree 配下 (同一を含む) ならエラーにする。従来はこのケースは「task に属していない」エラーだったので、
  エラーのまま維持する形になる。

### 3.2 取得の配管 (agentium.rs)

- 次を `pub` にする: `fetch_pr_list`, `PrFetchContext` (フィールドも), `PrFetchOutcome`, `PrInfo`,
  `ReviewEntry`, `ReviewDecision`, `CiInfo`, `fetch_issue_metadata_for`, `IssueMetadata`, `git_remote_url`,
  `parse_backlog_remote`, `BacklogRemote`, `remote_url_to_browser_url`, `fetch_ci_status`。
- `PrInfo`, `ReviewEntry`, `ReviewDecision` に `Serialize, Deserialize` を付ける (3.5 のスナップショットと
  CLI の表示に使う)。`PrInfo` と `ReviewEntry` には `PartialEq` も付ける (3.5 で変化の有無を比べる)。
- `fetch_pr_list` 内のローカル構造体 `GhPr` と、`fetch_backlog_pr_list` 内の `BeePr` / `BeePrStatus` を
  モジュール直下に出し、`PrInfo` への変換を `pr_info_from_gh(pr, reviews)` と
  `pr_info_from_bee(pr, &BacklogRemote)` に切り出す。既存の 2 関数はこれを呼ぶ形にする。
- 新規 `pub async fn fetch_pr_by_ref(reference: &PrRef) -> anyhow::Result<PrInfo>`:
  - GitHub: `gh pr view {n} --repo {repo} --json {JSON_FIELDS}` を `GhPr` に読み、`fetch_reviews` で
    レビューを取り、`pr_info_from_gh` に渡す。
  - Backlog: `bee pr view {n} -p {project} -R {repo} -s {web_host} --json number,summary,base,branch,status`
    を `BeePr` に読み、`PrRef` のフィールドから `BacklogRemote` を組んで `pr_info_from_bee` に渡す。
  - サブプロセスが失敗したら `bail!` する。`log::warn!` で握って空を返す既存の流儀は取らない。CLI 経路で
    認証切れが「PR なし」に見えるのを避けるため。
- `fetch_reviews` と `fetch_ci_status` の `working_dir` を `Option<&Path>` にし、None なら
  `current_dir` を設定しない。`fetch_ci_status` には `repo: Option<&str>` を足し、Some なら
  `--repo` を渡す。既存呼び出しは `Some(&working_dir)` と `None` を渡すだけの変更になる。

### 3.3 手動 PR を arena の PR 取得に合流させる (Rev 2)

手動 PR はタスク単位の別パイプラインを持たず、arena の `fetch_pr_list` に入力として渡す。以降の
`pr_info`、CI 連鎖、レビュー、セッション対応 (`save_pr_session_mapping`)、`pr_cache.json`、サイドバー描画は
既存のまま流れる。

- `pub fn pr_ref_matches_remote(reference: &PrRef, remote_url: &str) -> bool` を agentium.rs に置く。
  GitHub は origin を (host, owner/repo) に正規化 (`https://`、`git@host:`、`ssh://git@host/` の 3 形) して
  host が `github.com` かつ owner/repo が大文字小文字を無視して一致、Backlog は `parse_backlog_remote` の
  web_host / project / repo が一致、で判定する。CLI の add-pr 検証と fetch 側のフィルタで共用する。
- `PrFetchContext` に `task_pr_refs: Vec<PrRef>` を足し、`pr_fetch_context` は arena を含む全タスクの `prs` を
  詰める (CLI の有無は fetch 側で判断)。
- `fetch_pr_list` は `git_remote_url` を得た後、`task_pr_refs` を `pr_ref_matches_remote` で絞り、既存の
  provider 経路 (Backlog / GitHub) と `futures::join!` で並行に `fetch_pr_by_ref` を回す。PR ポーリングは
  1 周が 5 秒を超えると自動停止するので、手動分を直列に足してはいけない。
- 手動 1 件ごとの失敗は `log::warn!` して飛ばす。`fetch_pr_list` 全体を `Err` にすると
  `apply_pr_fetch_result` の `Err` arm が arena の発見済み PR まで消す。
- 結合は provider 経路が返った後に外側で行う。Backlog 経路は issue id が無いと早期 return し、GitHub 経路も
  `gh pr view` 失敗で早期 return するため、そこで手動分が落ちないようにする。番号が重複するものは発見分を
  優先し、番号順に並べる。provider の CLI が使えないときは手動分も取得しない。
- `handle_task_command` は `AddPr` を適用した後、そのタスクの live な arena すべてに `fetch_pr_for_arena` を
  呼ぶ。ポーリングはアクティブ arena しか回らないので、これが無いと非アクティブ arena の PR は切り替えるまで
  出ない。live な arena は `board_cache.task_worktrees` の `WorktreeRow::Live { arena_index }` から引く。
- `remove_pr_from_task(task_id, &PrRef, cx)` は `prs` から除いて `board_changed` し、同じく live arena を
  再取得してピルを消す。CLI コマンドは足さない。

### 3.4 アプリ側の描画 (Rev 2)

手動 PR は arena の `pr_info` に入ってくるので、タスク直下の専用行は無い。arena 行の PR ピルに 2 点だけ足す。

- `render_arena_row` の `pr_el` と `review_el` の組み立てを
  `render_pr_elements(&self, id_prefix, pr: &PrInfo, ci, branch, show_base, clickable,
  manual_link: Option<(Uuid, PrRef)>, cx: &Context<Self>) -> (AnyElement, Option<AnyElement>)` に切り出す
  (Rev 1 の `with_tooltip` は不要になったので外す)。
- Backlog バッジ: `pr.provider == PrProvider::Backlog` のとき、ピルの先頭 (状態アイコンの前) に
  `backlog_badge()` を置く。`render_issue_row` から切り出したものを共用する。これは発見された Backlog PR にも
  付くので、Backlog arena の既存行の見た目が変わる。ユーザーのモックが arena 行内のバッジを示していたため
  こうした。
- 右クリック: ピルに `on_mouse_down(MouseButton::Right)` を付け、`deploy_pr_context_menu` で
  Open in Browser と、手動リンクの PR のときだけ Remove from Task を出す。手動かどうかは
  `board_cache.task_worktrees` を `WorktreeRow::Live { arena_index }` で逆引きし、その task の `prs` に
  `html_url()` が一致する `PrRef` があるかで決める。`tasks_containing_arena` は canonicalize するので
  描画経路からは呼ばない。gh が返す `url` は `https://github.com/{repo}/pull/{n}` で、Backlog の URL は
  同じ書式で組むので `html_url()` との等値比較で足りる。

### 3.5 arena 由来 PR のスナップショット

`task info` が `--update` なしで arena の PR を出すために、アプリが取得結果を書き出す。

- ファイルは `paths::data_dir().join("pr_cache.json")`、中身は
  `HashMap<String, Vec<PrInfo>>` (キーは canonical な worktree パス)。
- `pub fn load_pr_cache()` / `pub fn write_pr_cache(&HashMap<..>)` を `load_pr_session_db` /
  `write_pr_session_db` (agentium.rs:3003 から 3027) と同じ形で用意する。
- アプリは起動時に `load_pr_cache()` で `pr_cache: HashMap<String, Vec<PrInfo>>` を持つ。
  `apply_pr_fetch_result` (agentium.rs:866) で、`Ok` かつ空でなければ該当パスを差し替え、`Ok` で空か
  `Err` ならキーを消す (`pr_info` と同じ意味論)。差し替え前後を `PartialEq` で比べ、変化があったときだけ
  `_pr_cache_write_task` で書く。ポーリングは 60 秒ごとに `apply_pr_fetch_result` を呼ぶので、無条件に
  書くと毎分ファイルが更新される。書き手はアプリだけなので board.json と同じ単一ライター規則に収まる。
- 制約: `start_pr_polling` はアクティブ arena しか回さないので、非アクティブ arena のスナップショットは
  最後の単発取得 (arena 作成、切替、HeadChanged、Claude 完了) の時点で止まる。`task info` の出力では
  スナップショットの取得時刻を添えず、代わりに `--update` を促す一行を末尾に出す。

### 3.6 CLI (main.rs)

`TaskAction` に 2 つ追加する。

```rust
/// Show the task containing the current directory (arenas, PRs, issues)
Info {
    #[arg(long)]
    task: Option<String>,
    /// Re-fetch PR and issue metadata before printing
    #[arg(long)]
    update: bool,
},
/// Link a pull request to a task
AddPr {
    /// PR reference (URL, owner/repo#123, or a bare number resolved against the current repo's origin)
    pr: String,
    #[arg(long)]
    task: Option<String>,
},
```

- `Command::Task` 分岐の先頭で `zlog::init(); zlog::init_output_stderr();` を呼ぶ。ライブラリ側の
  `log::warn!` が stderr に出るようにするため。GUI 経路の `zlog::init()` はこの分岐が return した後なので
  二重初期化にはならない。
- `run_task_info`:
  1. `board::load_board()`、`resolve_target_task` (3.1 の祖先照合版) で task を決める。
  2. `--update` のときだけ、`gh --version` / `bee --version` の同期プローブで可用性を取り、
     `std::env::set_var("GH_PROMPT_DISABLED", "1")` を設定する (CLI は stdin が tty なので、gh が
     対話に入ると止まる)。
  3. issue: `IssueLink` のキャッシュを出す。`--update` なら `smol::block_on(join_all(fetch_issue_metadata_for))`
     の結果で上書きして出す。書き戻しはしない。
  4. (Rev 2) 手動 PR の独立した節は置かない。arena 節の PR 行に合流し、`task.prs` のいずれかと
     `html_url` が一致する行には `(linked)` を付ける。どの worktree の origin にも一致しない `PrRef`
     (Rev 1 の実装で登録された孤立エントリなど) は、arena 節の後に `unlinked:` として列挙する。
  5. arena: `task.worktrees` ごとに、パス、存在有無、ブランチ (`git rev-parse --abbrev-ref HEAD`)、
     短い HEAD、origin URL、provider (`parse_backlog_remote` で判定) を同期 `std::process::Command` で
     取る。PR は `load_pr_cache()` を canonical パスで引く。`--update` なら
     `fetch_pr_list(path, PrFetchContext { gh_available, bee_available, task_issue_keys: task の Backlog
     issue key, task_pr_refs: task.prs, known_issue_ids: 空, failed_issue_keys: 空 })` を
     `smol::block_on` で呼ぶ。手動 PR もこの経路で取れる。
  6. 出力は 1 行 1 項目のプレーンテキスト。形の例 (task 名、UUID、パス、HEAD は仮の値。PR と issue は
     `bee pr view` で確認した実データ):

```
[<index>] <task title>  (<uuid prefix>)
issues:
  BLG_AI_INTEGRATION-324  完了   Mastra 1.0までの変更に追従する   https://nulab.backlog.jp/view/BLG_AI_INTEGRATION-324
arenas:
  <worktree path>   BLG_AI_INTEGRATION-324/tool-call-error @ <short sha>   origin: nulab.backlog.jp (backlog)
    backlog  #163  closed  main   Mastra 1.0までの変更に追従する  (linked)
(PR/issue は前回取得時点の値です。最新にするには --update)
```

- `add-pr`:
  1. `input.parse::<u32>()` が通れば素の番号。`smol::block_on(git_remote_url(&cwd))` の origin を
     `parse_backlog_remote` に通し、通れば `PrRef::Backlog`。通らなければ `remote_url_to_browser_url` の
     結果を見て、host が `github.com` のときだけ `https://github.com/{owner}/{repo}` として `PrRef::GitHub`。
     `remote_url_to_browser_url` は host を見ずに `https://` を組むので、この host 判定を省くと GitLab
     などの origin が GitHub 扱いになる。どちらでもなければ「origin から provider を推定できない」エラー。
  2. それ以外は `board::parse_pr_ref`。
  3. (Rev 2) 検証: 解決した task の `worktrees` のうち存在するものについて `git remote get-url origin` を
     取り、`pr_ref_matches_remote(&reference, origin)` がどれか一つでも真でなければエラーにする。
     メッセージには task の origin 一覧を載せる。この検証は IPC 経路と直接書き込み経路の両方の前で行う。
  4. `dispatch_task_commands(vec![TaskCommand::AddPr { task_id, pr: reference }])`。
     envelope は 300 バイト前後で datagram 上限 1900 に余裕がある。
  5. `added {label} to task {task_id}` を出す。表示への反映はアプリ側 (`handle_task_command` が該当 task の
     live arena に `fetch_pr_for_arena` を呼ぶ) に任せる。アプリが居なければ次回起動時の取得で出る。

### 3.7 変更しないもの

- `fetch_pr_list` のブランチ駆動の発見ロジックと、`log::warn!` で続行する既存の流儀。
- `pr.json` (PR とセッションの対応) の形式と書き方。
- `IssueLink` の形式。
- arena 行の見た目 (Rev 2 で Backlog PR の B バッジと右クリックメニューだけ足した。それ以外は同じ)。

## 4. 実装ステップ

各ステップ単独でビルドが通る順に並べる。

0. README.md の先頭に `> [!IMPORTANT]` と確認行を付ける (crates/agentium/CLAUDE.md の必須手順)。
1. board.rs: `PrStatus` / `PrProvider` の移動と再エクスポート、`PrRef` / `PrLink` / `prs`、
   `TaskCommand::AddPr` と `apply`、`parse_pr_ref`、`tasks_containing_path`、テスト。
2. agentium.rs 配管: `pub` 化、serde derive、`GhPr` / `BeePr` のモジュール直下への移動と変換関数の
   切り出し、`fetch_pr_by_ref`、`fetch_reviews` / `fetch_ci_status` の引数変更。
3. agentium.rs 合流 (Rev 2): `pr_ref_matches_remote`、`PrFetchContext.task_pr_refs`、`fetch_pr_list` 内での
   手動 PR の並行取得と結合、`handle_task_command` / `remove_pr_from_task` からの `fetch_pr_for_arena`。
4. agentium.rs 描画 (Rev 2): `render_pr_elements` / `backlog_badge` の切り出し、ピル先頭の Backlog バッジ、
   右クリックの `deploy_pr_context_menu` (手動 PR のみ Remove from Task)。
5. agentium.rs スナップショット: `load_pr_cache` / `write_pr_cache`、`pr_cache` フィールド、
   `apply_pr_fetch_result` からの書き出し。
6. main.rs: `TaskAction::Info` / `AddPr`、zlog 初期化、`resolve_target_task` の祖先照合、
   `run_task_info`、add-pr の番号解決。
7. README.md の `agentium task` 節に 2 コマンドと `pr_cache.json` の説明を足す。
8. 検証 (5 章)。

## 5. テスト計画

board.rs のユニットテスト:

- `parse_pr_ref` が GitHub URL (`/pull/N`、`/pull/N/files`、クエリ付き)、Backlog URL、`owner/repo#N` を
  受け、issue URL (`/issues/N`) と素の番号と無関係な文字列を拒否する。
- `TaskCommand::AddPr` が `serde_json::Value` 経由と文字列経由で往復する (既存
  `task_command_round_trips_through_value` と同じ経路)。
- `Board::apply(AddPr)` が同じ reference を 2 回受けても 1 件のまま。
- `PrRef` が Rev 1 の `PrLink` 形 (title / status の余分なフィールド付き) の JSON からも読める。
- `PrStatus` が小文字の JSON と往復する。
- (agentium.rs) `pr_ref_matches_remote` が GitHub の 3 形式 (`https://`、`git@`、`ssh://`) と Backlog の
  origin で真、別リポジトリと GitLab host で偽。
- `tasks_containing_path` が worktree のサブディレクトリを渡されたときに親の task を返し、
  worktree 直下と同じ結果になる。無関係なパスは空。
- 既存の `resolve_task_by_index_uuid_prefix_and_title` は `BoardTask` にフィールドが増えるので
  `prs: Vec::new()` を足す。

main.rs 側は同期 CLI 関数なので `#[cfg(test)]` を足すより手動確認に寄せるが、`add-arena` のガードだけは
純粋関数 (`path_is_inside_task_worktrees(task, path) -> bool`) に切り出してユニットテストを書く。

手動確認:

1. `cargo test -p agentium` と `./script/clippy` が通る。
2. arena 行の PR 表示が切り出し前と同じ (GitHub で CI とレビューが出る arena、Backlog の arena)。
3. backlog-ai-agent の arena を含む task で
   `agentium task add-pr https://nulab.backlog.jp/git/BLG_AI_INTEGRATION/backlog-ai-agent/pullRequests/163`
   を実行すると、その arena 行に B バッジ付きの `#163` が closed 色で出る (アクティブでない arena でも
   すぐ出る)。右クリックで Open / Remove from Task が動き、Remove でピルが消える。
4. GitHub PR を `owner/repo#N` で add-pr し、arena 行の発見された PR と同じ見た目 (状態アイコン、CI、
   レビュー要素) で並ぶ。同じ PR が発見分にもある場合は 1 つだけ出る。
5. task のどの arena とも origin が一致しないリポジトリの PR を add-pr するとエラーになり、board.json は
   変わらない。
6. `agentium task info` を worktree のサブディレクトリから実行して task が解決される。
   `--update` あり / なしで PR と issue の値が出る。gh を `gh auth logout` した状態で `--update` すると
   stderr にエラーが出る。
6a. worktree のサブディレクトリで `agentium task add-arena` (引数なし) を実行するとエラーになり、
   board.json に新しい worktree が増えない。
7. アプリ未起動で `add-pr` すると board.json に直接書かれ、次回起動時にメタデータが埋まる。

## 6. リスクと未決事項

- `ReviewEntry` / `CiInfo` のフィールドに serde を実装していない型があれば、そこにも derive が要る。
  ステップ 2 のビルドで判明する。
- `bee pr view` の `status.name` が Open / Merged / Closed 以外を返した場合は既存と同じく warn して
  Open 扱いにする。
- `fetch_pr_list` には内部タイムアウトがない。アプリ側は 5 秒ガードがあるが CLI にはない。
  `GH_PROMPT_DISABLED=1` で対話待ちは防ぐが、ネットワーク待ちはそのまま待つ。
- `pr_cache.json` は非アクティブ arena について古くなる (3.5)。`task info` の末尾で `--update` を
  案内する以上の対策はしない。
- サイドバーは 2 秒ごとに再描画される。`render_task_pr_row` はメモリ上の値だけを使い、
  ファイルシステムには触らない。
- `PrStatus` / `PrProvider` の移動で agentium.rs 内の参照はすべて再エクスポート経由になる。
  `use` の重複が clippy に引っかかったら整理する。

## 7. 見積り

| 範囲 | 目安 |
|---|---|
| board.rs (モデル、parse、祖先照合、テスト) | 250 行 |
| agentium.rs 配管 (pub 化、serde、変換関数、`fetch_pr_by_ref`) | 150 行 |
| agentium.rs データ (`fetch_task_pr_metadata`、remove) | 120 行 |
| agentium.rs 描画 (切り出し、task PR 行、メニュー、重複排除) | 200 行 (うち移動 150 行) |
| agentium.rs スナップショット | 50 行 |
| main.rs (info、add-pr、zlog、祖先照合) | 200 行 |
| README | 20 行 |
