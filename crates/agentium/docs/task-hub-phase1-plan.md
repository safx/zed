# タスクハブ フェーズ 1（限定自動）の実装プラン

Rev 5 (2026-11-16)

改訂履歴:

- Rev 5: 実装とコードレビュー（Codex、REQUEST CHANGES、P2×3）で決めたこと。(1) ack 済みのバッチは、
  ハブが人間との会話などで動き続けていてもタイムアウトにしない（送信からの 900 秒は ack 前だけ）。
  (2) `SerialWriter` は書き込みが終わった時点で失敗をセルに記録する。`Shared` は誰かが poll しないと
  結果を返さないため、tick が書き込みの失敗に気づけなかった。(3) 書きかけの判定は `capture_key_down`
  に加えて、キーに割り当てられたアクション（`terminal::Paste`、`editor::actions::Paste`、
  `terminal::PasteText`、`SendText`、`SendKeystroke`）も入力として記録する。GPUI はキーに割り当て
  られたアクションをキーのリスナーより先に処理するため、Cmd+V の貼り付けを見逃していた。
  (4) ack の手順は `AckHost` trait 越しにアプリへアクセスし、偽のホストで、To do・質問の書き込み失敗、
  各 await 中の世代の変化、カーソルの保存失敗、処理中の 2 度目の ack を試す。
  (5) 実機確認（Claude Code 2.1.325）: 貼り付けた文面は UserPromptSubmit の prompt に全文で届くが、
  `\n\n<\pasted_content id="…">\n` で包まれる。ハブが起こしたターンの判定（`started_by_hub`）はこの
  開きタグを読み飛ばす。読み飛ばさないと、ハブの回答のたびに連続回答の数がリセットされ、上限が効かない。
  ほかに実機で確かめたこと: 長いバッチの貼り付けが送信まで進む、許可した `agentium` コマンドが確認なしで
  通る、`hub answer` が質問者に届き「Answered by Task Coordinator」になる、ハブはハブのタブで人間が
  言ったことを根拠に答え、根拠のない質問は人間に残す、`--permission-mode default` はユーザー設定の
  `defaultMode: auto` より優先される、Coordinator から（`!` で直接）実行した `tab send-message` は
  Agentium が拒否する。
  (6) ハブはコードを読まない。指示文が「コードで決まる質問に答える」と読めたため、ハブが自分で調べて
  いた。ハブは指示するだけの役なので、調べる必要があれば担当のエージェントへの提案として起票する。
  あわせて `--settings '{"sandbox":{"autoAllowBashIfSandboxed":false}}'` で起動する。ユーザー設定の
  `autoAllowBashIfSandboxed: true` があると、サンドボックス内のコマンドは permission mode によらず
  確認なしで通り、`agentium.sock`（`pane split` でサンドボックス外のコマンドを起動できる）にも届くため。
  ユーザー設定の個別の allow ルール（`rg`、`grep`、`git log` など）は残るので、指示文に従わないハブは
  それらでコードを読めてしまう（強制ではない）。実機では、ハブが `--settings` 付きで起動していること
  （`ps`）と、ハブが `agentium` 以外のコマンドを断ることまで確かめた。`--settings` が
  ユーザー設定の `autoAllowBashIfSandboxed: true` より優先されるかは、ハブがコマンドを試さないため
  未確認。

- Rev 4: Codex レビュー（REQUEST CHANGES、P1×2、P2×3）を反映。(1) ack は検証から保存・最終確認までを
  1 つの共有タスクにし、2 度目の ack はそれを待つ（§3.5）。(2) カーソルはメモリへ先に入れず、自分の
  書き込みが成功してから反映する。失敗時の戻しは不要になる（§3.5）。(3) To do と質問の書き込み失敗は
  Coordinator のエラーとして表示し、Retry で書き直す（§3.5）。(4) Coordinator からの hook 通信は、
  要求のセッションが Coordinator 自身の `claude_session` と一致するときだけ通す（§3.6）。(5) Codex の
  指示の送り方を「要約のみでは毎ジョブ全文」に統一（§3.3）。

- Rev 3: Codex 再レビュー（REQUEST CHANGES、P1×3、P2×4）を反映。(1) To do・質問・Store の書き込みは
  結果（成功か失敗か）を共有タスクで返し、ack は失敗ならカーソルを進めない（§3.5）。(2) ack は最後の
  保存の後にもバッチ id と世代を確かめ、保存中の 2 度目の ack は同じ結果を待つ。失敗したらメモリの
  カーソルを戻す（§3.5）。(3) 保証の範囲を「識別できた Coordinator の操作制限」に絞る。限定自動は
  承認方式を起動時に指定できる Claude のハブだけで有効にし、Codex Luna はフェーズ 0 の確認まで
  要約のみ（§3.2、§3.6）。(4) 許可リストに Coordinator 自身の hook 通信（`coordinator_claude_turn`、
  自分のセッションの `permission_request`）を入れる（§3.6）。(5) `task info --task <id>`（§3.10）。
  (6) タブが復元されていない記録も配り、取れない表示項目は null にする（§3.5）。(7) 保存と ack の
  非同期の制御を切り出してテストする（§4）。

- Rev 2: Codex プランレビュー（REQUEST CHANGES、P1×5、P2×5）を反映。(1) Coordinator からの要求は
  サーバー側の許可リストで絞り、peer の祖先探索を深くする（§3.6）。(2) 回答の直前に、送り先のタブの
  セッションが質問者と同じか確かめる（§3.8）。(3) 記録ごとに配るタスクを作成時に保存し、配送・回答の
  認可・再起動で共用する（§3.5）。(4) ack は、処理中の To do と質問の書き込み、カーソルの保存を待って
  から返す。世代を確かめる（§3.5）。(5) 共通の送信関数で、貼り付けの前に端末を同期する（§3.8）。
  (6) 確認済みの記録は送らない。完了記録の arena_path によるフォールバックはやめる（§3.5）。
  (7) id の下限は To do の読み込みより前に適用し、カーソルを `acked_inbox_id` と呼ぶ（§3.4）。
  (8) 出来事だけでも起動する。バッチに task_id とアリーナを入れ、JSON の形を決める（§3.5）。
  (9) Coordinator の `todo assign` は自分のタスクの項目だけ（§3.9）。(10) テストを追加（§4）。

設計の正本は repo 外の `agentium-task-hub-design.md`（別セッションとユーザーが確定したもの）。この文書は、
そのうちフェーズ 1 だけを、コードに落とせる粒度まで決める。パスは `crates/agentium/src` からの相対。

## 1. 目的とスコープ

Task Coordinator（タスクごとに 1 つ、ペインに置かない要約セッション）を、人間が指示を出せる「ハブ」に
広げる。フェーズ 1 では、ハブが単独でしてよいことを「配下の `todo ask` への回答」と「To do の起票と
担当付け」に限る。Go と自由文の送信は人間が行う。

含むもの:

1. モード（要約のみ / 限定自動）とモデル（Haiku / Sonnet / Opus / Codex Luna）のタスクごとの設定。
   フェーズ 1 の限定自動は Claude のハブ（Haiku / Sonnet / Opus）だけ。Codex Luna は要約のみ（§3.6）。
2. 出来事の配送: Inbox の記録（質問、完了、失敗、CI 失敗）と要約ジョブを 1 バッチにまとめてハブへ送る。
   `agentium hub ack` で処理済みにする。
3. `agentium hub answer`: 質問した本人のタブに回答を送る。連続回答の上限つき。
4. Coordinator からの要求の許可リストと、`tab send-message` などの拒否。
5. `todo assign` の送り先を、項目のタスクのタブに限る。
6. Inbox id の再利用の修正（カーソルが前提とするため）。
7. 人間の書きかけとの衝突回避（Claude: キー入力の時刻と IME、Codex ACP: 事前確認）。
8. モード別の INSTRUCTIONS、スキル、README の更新。

含まないもの（フェーズ 2 以降）: `hub go`、`hub send`、完全自動モード、ループの歯止め、
`hub notify`（Coordinator から `todo add --user-task` が使えるため不要。実機で使えなかった場合だけ足す）。

要約のみモードの挙動は変えない（毎回の全文指示、1 ジョブずつ、300 秒、要約の登録か差し替えで完了、
ack 不要）。例外は 2 つで、両モードに効かせる: §3.7 の書きかけ保留、§3.8 の送信前の端末同期。

## 2. 現状（コード上の事実）

- 要約ジョブ: `Saved.jobs: BTreeMap<owner, Job>`（task_coordinator.rs:195）。owner ごとに上書き
  （:87-125）。送信は 1 件ずつ、要約の登録で完了（:1010-1016）、300 秒でエラー（:1017-1022）。
  毎回 INSTRUCTIONS 全文を前置する（:1038）。自動起動の条件は未要約ジョブの存在（:917-929）。
- Codex のハブには system prompt を渡していない（:1145-1160）。指示は毎回のジョブ本文だけで届く。
- Provider は ClaudeHaiku / CodexLuna の 2 つ（:28-32）。`--model haiku` 固定（:1096）。UI は 2 択の
  トグル（:1272-1300）。Provider を網羅する match は Overview にもある（agentium.rs:8545）。
- Inbox の記録: 質問 `InboxQuestion`（agentium.rs:517）、完了と失敗と CI 失敗 `InboxCompletion`
  （:595）。id は To do と共有のカウンタ `next_inbox_question_id`。起動時に「残っている記録の最大 + 1」
  で決まる（:430-442、:1568、:1589）。確認済みの完了記録（:1564）と 30 日前に Done の To do
  （inbox_todo_view.rs:535-543）は起動時に消えるので、id が再利用されうる。To do の旧形式の移行は
  :1568-1588 の `InboxTodos::load` の中で id を発行する。
- 完了記録はタブが Busy になると消え（agentium.rs:4573-4590）、確認済みは 1 時間メモリに残る（:709-729）。
  未回答で assume でない質問を出したタブは、完了として記録されない（失敗は記録される。:4510-4515、:671）。
  通常の完了記録はアリーナのパスを持たない（`pull_request.arena_path` は CI 用だけ）。
- 質問への回答: `answer_inbox_question`（:4877-4931）。回答済みや範囲外は何もせず戻る（:553-579）。
  送信に成功したとき、または assume の追認（送信しない）のときに `resolution` を書く。
  回答先はタブの生存だけで決まり、そのタブのセッションが質問者と同じかは確かめない（:4358、:4914）。
- 共通の送信関数 `send_message_to_item`（:3393-3419）は貼り付けの前に `Terminal::sync` を呼ばない。
  未描画の端末では bracketed paste の状態が古く、最初の改行で送信されてしまう。Coordinator は自分で
  同期している（task_coordinator.rs:1042-1049）。
- 呼び出し元: `tab_send_message` は `tab_self(peer_ancestor_pids)` だけで検証し、Coordinator は
  unverified になる（main.rs:2306-2309）。`tab_new` は任意のコマンドでタブを作れる（main.rs:2331）。
  peer の祖先は最大 10 段（main.rs:2068-2083、:2125-2133）。`pane split` は peer 情報の無い
  データグラム経路。
- `todo assign --to <title> --arena <path>` は、項目のタスクと無関係なアリーナのタブも担当にでき、
  呼び出し元のタスクと項目のタスクも照合しない（agentium.rs:3734-3754）。
- To do と質問の書き込みは別々の非同期列で、失敗はログだけ（inbox_todo_view.rs:1060-1078、
  agentium.rs:4854-4872）。Coordinator の Store も別の列で、失敗は `error` に残る
  （task_coordinator.rs:324-356）。
- `task info` は selector か cwd から、`tab list` はアリーナから対象を決める（agentium.rs:5004、:3319）。
  Coordinator は共通ディレクトリで動くので、引数なしでは自分のタスクを引けない。
- 書きかけ: Claude TUI のハブには書きかけを知る手段がない。Codex ACP は書きかけがあると
  `insert_external_message` がエラーを返し（agent_ui/src/conversation_view/thread_view.rs:1747）、
  task_coordinator.rs:1054-1056 がそれを `runtime.error` にするので、Retry まで止まる。

## 3. 設計

### 3.1 保存形式

`Coordinator`（task_coordinator.rs:58）に 2 項目を足す。どちらも `#[serde(default)]`。

```rust
pub mode: Mode,            // SummaryOnly（既定） | LimitedAuto
pub acked_inbox_id: u64,   // ハブが ack したバッチの上限 id。後退しない
```

`acked_inbox_id` を進めるのは、保存に成功した ack だけ。例外は限定自動へ切り替えたときの初期化
（§3.2）で、これは送っていない記録を読み飛ばすための設定であり、配送済みの意味ではない。

`Provider` に `ClaudeSonnet`、`ClaudeOpus` を足す（serde は変種名なので既存値と互換）。
`Saved.version` は 2 のまま。

`InboxQuestion` と `InboxCompletion`（agentium.rs:517、:595）に `hub_task: Option<Uuid>` を
`#[serde(default)]` で足す。記録を作るときに決め、以後は変えない（§3.5）。

### 3.2 モードとモデルの切り替え

- 変更できるのは Disable 中だけ（今の `provider_locked` を「enabled のとき」に緩める）。
- モードかモデルを変えたら、セッションを新しくする（`claude_session` を振り直し、`claude_started`、
  `codex_id`、`codex_tab` を消す。今の Create new session と同じ処理を関数にまとめて共用）。
  `--append-system-prompt` は起動時にしか効かないので、モードと system prompt を常に一致させる。
- 限定自動に切り替えたとき、Provider が ClaudeHaiku なら ClaudeSonnet にする。要約のみに戻しても
  Provider は変えない。
- 限定自動は Claude の Provider でだけ選べる。限定自動の間、Provider のボタンは Luna を飛ばして回す。
  保存されたファイルに CodexLuna と LimitedAuto の組が入っていたら、要約のみとして扱う。
- 限定自動に切り替えたとき、`acked_inbox_id` を「その時点の最大発行済み id」
  （`next_inbox_question_id - 1`）にする。切り替え前の記録はハブに送らない。
- UI: provider ボタンを「Haiku → Sonnet → Opus → Luna」と回すボタンにし、隣に
  「Summary only / Limited auto」のボタンを置く。どちらも enabled の間は無効。Overview の
  Provider の match（agentium.rs:8545）も直す。

### 3.3 起動

- Claude: `--model` を Provider から（haiku / sonnet / opus）。`--append-system-prompt` と
  `--allowedTools` をモードから選ぶ。限定自動の allowedTools は、今の 4 つに
  `Bash(agentium hub answer *)`、`Bash(agentium hub ack *)`、`Bash(agentium task info *)`、
  `Bash(agentium tab list *)` を足す。
- Claude の限定自動では、さらに `--permission-mode default` を渡し（ユーザー設定の既定モードが
  bypass でも、許可リスト外の Bash は確認を出す）、`--disallowedTools` で
  `Bash(agentium tab send-message *)`、`Bash(agentium tab new *)`、`Bash(agentium tab log *)`、
  `Bash(agentium pane *)` を拒否する（deny は allow より優先）。CLI の指定がユーザー設定より優先されるかは
  フェーズ 0 で確認する。
- Codex: 起動は変えない（system prompt を渡す口がない）。フェーズ 1 の Codex は要約のみなので、
  今どおり毎ジョブ INSTRUCTIONS 全文を前置する。
- 自動起動の条件（task_coordinator.rs:926）に、限定自動では「配れる記録がある」を加える。

### 3.4 Inbox id の再利用を直す（agentium.rs）

- 不変条件: 「どの Coordinator の `acked_inbox_id` 以下の id も、二度と発行しない」。発行済み id 全体の
  再利用禁止ではない（ハブの判定に関わるのはカーソル以下だけなので、これで足りる）。
- 起動時の最初の計算（:1568）を `max(next_inbox_id(...), 全 Coordinator の acked_inbox_id の最大 + 1)`
  にする。`InboxTodos::load` の旧形式の移行（inbox_todo_view.rs:485 付近）より前なので、移行で振る id にも
  効く。Store は :1281 で先に読み込まれている。
- `next_inbox_id` に下限の引数を足し、単体テストで確かめる。

### 3.5 出来事のバッチ（限定自動モードだけ）

要約のみモードは今の流れのまま。限定自動モードの `coordinator_tick_task` は次の流れにする。

配るタスクの決め方（記録の作成時に 1 回だけ）:

- 質問: `todo_ask`（agentium.rs:3471）で、質問したタブのアリーナから `todo_task_for_arena`。
- 完了と失敗: `sync_inbox_completions` で `FinishedTab` を作るとき、そのアリーナから同じく。
  `record_finished_tabs` が `InboxCompletion` に写す。
- CI 失敗: 記録を作るとき（agentium.rs:4640 付近）、`arena_path` のアリーナから同じく。
- 決めた値は `hub_task` に保存し、選択中のタスクが後で変わっても変えない。配送、`hub answer` の認可、
  再起動後の配送のすべてがこの値を使う。値の無い既存の記録は配らない。

バッチに入れるもの（送る時点で残っているもの。ログではなくスナップショット）:

- events: `hub_task` がこのタスクで、id が `acked_inbox_id` より大きいもの。
  - 質問: 未回答のもの。
  - 完了と失敗: `asker` があり（Claude のセッションか Codex ACP）、`confirmed_at` が None のもの。
  - CI 失敗: `pull_request` があり、`confirmed_at` が None のもの。
- summaries: このタスクの未要約の要約ジョブ全部（今の observe のまま作られる）。
- events も summaries も空なら送らない。

人間が先に回答・確認した記録や、タブが動き出して消えた完了記録は送らない（仕様）。

JSON の形:

```json
{
  "batch_id": "uuid",
  "task": { "id": "uuid", "title": "…", "arenas": [{ "path": "/…", "name": "…" }] },
  "events": [
    { "type": "question", "id": 41, "agent": { "title": "Claude", "arena": "/…", "kind": "terminal" },
      "question": "…", "context": "…", "options": [{ "number": 1, "label": "…", "description": "…" }],
      "recommended": 1, "assumed": false },
    { "type": "completed" | "failed", "id": 42, "agent": { … }, "title": "…", "detail": "…", "failure": null },
    { "type": "ci_failed", "id": 43, "arena": "/…", "pull_request": 2791 }
  ],
  "summaries": [{ "job_id": "uuid", "request": "…", "response": "…" }]
}
```

選択肢の番号は 1 始まり（CLI の `--recommended` と同じ）。`task.arenas` は `tab list --arena` と
`todo assign --arena` に使い、`task.id` は `task info --task <id>` に使う。

`agent` の各項目は、記録の生きているタブ（無ければ asker のタブ）から取る。再起動直後などでタブが
見つからない記録も配り、そのときは `title`・`arena`・`kind` を null にする。記録に保存されている
`arena_name`（質問と完了の両方にある）は `agent.arena_name` として常に入れる。表示項目が取れない
ことを理由に記録を落とさない。

送信:

- `Runtime.sent` に `SentBatch { id, max_inbox_id, job_ids, at, acked, launch_token }` を持つ
  （要約のみモードは今の `(job_id, at)` のまま）。`max_inbox_id` は events の最大 id、events が空なら
  今の `acked_inbox_id`（カーソルを後退させない）。送信後に発生した記録は、id がこれより大きいので
  次のバッチに入る。
- 文面: 起動後の最初のバッチだけ、限定自動の INSTRUCTIONS 全文を前置する（`Runtime.instructed`）。
  `--resume` で `--append-system-prompt` がどう扱われるかが実機未確認なため。2 回目以降は短い定型文と
  JSON だけにする。
- 次のバッチを送るのは「前のバッチが ack 済み、かつハブが idle か ready」のとき。
- タイムアウトは限定自動で 900 秒。超えたら今どおり error にし、Retry まで止める。

書き込み結果の共有:

- 小さな部品 `SerialWriter` を作る。`write(path, bytes, cx)` は前の書き込みを待ってから書き、
  `Shared<Task<Result<(), Arc<str>>>>` を `last` に持つ。`last()` で最後の書き込みの結果を待てる。
  後の書き込みは前の内容を含むスナップショットなので、最後の書き込みが成功していれば、それまでの
  変更はすべてディスクにある。
- To do（inbox_todo_view.rs:1060-1078）と質問（agentium.rs:4854-4872）の書き込みをこれに置き換える。
  失敗のログは今どおり残す。
- Store（task_coordinator.rs:324-356）に `write_with(f)` を足す。`saved` を複製したものに `f` を当てて
  書き、その書き込みの共有タスク（結果つき）を返す。メモリの `saved` は変えない。共有の `error`
  （後の保存で上書きされる）とは別に、ack は自分の書き込みの結果を見る。
- 限定自動の tick は、To do と質問の `last()` が失敗していれば `runtime.error` に
  「Could not save To do / questions: …」を出して配送を止める。Retry は今の Store の再保存に加えて、
  To do と質問を今のスナップショットで書き直す（`InboxTodos::persist`、`persist_inbox`）。

`agentium hub ack --batch <id>`（非同期で返す。`task_info` と同じく `Task<Value>`）:

1. 呼び出し元が Coordinator で（§3.6）、そのタスクが限定自動であること。
2. `<id>` が `Runtime.sent` のバッチと一致すること。一致しなければ「unknown or superseded batch」。
   `Runtime` は永続化しないので、再起動前のバッチの ack はこのエラーになり、同じ範囲が再送される
   （少なくとも 1 回届く）。
3. バッチの ack の状態（`SentBatch.ack: AckState`）で分ける。
   - `Done`: 成功を返す（保存まで成功した ack の 2 度目）。
   - `Running(shared)`: 同じ共有タスクの結果を待って返す（処理中の 2 度目）。
   - `None`: 4 以降の処理全体を 1 つの共有タスクにして `Running` に入れ、その結果を返す。共有タスクを
     入れるのは最初の await より前なので、2 度目の ack が別に保存へ進むことはない。
4. バッチの要約ジョブがすべて、要約済みか差し替え済み（id がもう無い）であること。残っていれば、
   その job id を並べたエラーで終わる。
5. To do と質問の `SerialWriter::last()` を待つ。どちらかが失敗なら、カーソルを進めずに
   「could not save To do / questions: …」で終わる。ハブが起票・回答した結果がディスクに載る前に
   カーソルだけが保存され、再起動で失われるのを防ぐ。
6. 待った後、タスクが実行可能で限定自動のまま、`Runtime.sent` の id が `<id>` と同じ、`launch_token` も
   同じであることを確かめる（Retry はバッチを捨て、Disable と新しいセッションは Runtime を作り直すので、
   古い ack が新しい状態を書き換えない）。違えば「superseded」で終わる。
7. `Store::write_with` で、`acked_inbox_id = max(今の値, max_inbox_id)` を当てたスナップショットを書き、
   その結果を待つ。メモリの `acked_inbox_id` はまだ変えない。
8. 書き込みが失敗したら、そのエラーで終わる（メモリは変えていないので戻す必要はない）。成功したら、
   メモリの `acked_inbox_id` を `max(今の値, max_inbox_id)` にして `Store::change` を呼ぶ（7 の後に並んだ
   別の保存が古いカーソルで書いていても、これで書き直される）。その後、6 と同じ確認が通れば成功、
   通らなければ「superseded」で終わる（カーソルは保存済みで正しい）。
9. 終わったとき、状態がまだ同じ `Running` なら、成功は `Done`、それ以外は `None` に戻す。

カーソルとディスクの関係: 7 の書き込みと 8 の反映の間に別の保存が並ぶと、ディスクのカーソルが一時的に
戻ることがある。8 の `Store::change` で書き直されるまでの間にアプリが落ちると、同じ範囲がもう一度
届く（重複）。カーソルがディスクで先に進んで出来事を失うことはない。

再送での重複: 再起動などで同じ出来事がもう一度届くことがある。INSTRUCTIONS で「起票の前に
`todo list` で同じ項目が無いか確かめる」と指示する。回答済みの質問への `hub answer` は無害に失敗する。

### 3.6 Coordinator からの要求の制限（agentium.rs、main.rs）

- `pub fn cli_caller(&self, peer_ancestor_pids, cx) -> CliCaller` を足す。
  `CliCaller::{Coordinator { task: Uuid }, Tab { title: String }, Unknown}`。判定はカーネルが確かめた
  peer の pid だけで行う（`coordinator_for_ancestors` → `tab_self`）。
- `handle_cli_request`（main.rs:2230）の入口で、呼び出し元が Coordinator なら許可リストの要求だけを
  通す。
  - 許可: `coordinator_summary`、`coordinator_claude_turn`（Coordinator 自身の UserPromptSubmit /
    Stop hook。main.rs:806）、`permission_request`（Coordinator 自身の PermissionRequest hook。
    main.rs:944。`inbox_permission_request` がセッションの Claude の pid と peer の祖先の一致を既に
    確かめている。他者の権限確認に答える操作ではない）、`hub_answer`、`hub_ack`、`todo_add`、
    `todo_assign`、`todo_list`、`task_info`、`task_list`、`tab_list`、`tab_self`。
  - 拒否: それ以外（`tab_send_message`、`tab_new`、`tab_log`、`todo_ask`、`todo_take`、`todo_done`、
    `todo_update`）。「the Task Coordinator cannot <type>; use `agentium hub` commands」。
  - hook 通信の照合: `coordinator_claude_turn` と `permission_request` は、要求の `session_id` が、
    識別した Coordinator の `claude_session` と一致するときだけ、状態を変える前に通す。一致しなければ
    「the Task Coordinator may only report its own session」で拒否する（他のセッションの状態や祖先 PID を
    上書きさせない。`inbox_permission_request` は未登録のセッションを照合しないため、ここで止める）。
- `hub_answer` と `hub_ack` は呼び出し元が Coordinator でなければ拒否する。
- peer の祖先探索の上限を 10 段から 32 段にする（`ancestor_pids_from`）。識別漏れを減らすためで、
  32 段を超える木や探索の失敗では今どおり Unknown になる。
- `tab_self` と `tab_for_ancestors` は変えない。

保証の範囲:

- フェーズ 1 が保証するのは「識別できた Coordinator の要求は、許可リストの操作しかできない」ことまで。
  限定自動モードの送信先が「質問した本人だけ」になるのは、この範囲での話。
- 識別できない経路が 2 つ残る。
  - Unknown の呼び出し元（祖先に Coordinator もタブも見つからないプロセス）は、今どおり
    `tab send-message` を unverified として送れる（main.rs:171-175 の既存の仕様。Codex の app-server の
    ようにタブの外で動くプロセスのため）。
  - `pane split`（データグラム経路）は peer を識別できない。
- ハブがこれらを使うには、許可リストにないコマンドを実行する必要がある。そこで限定自動は、承認方式を
  起動時に指定できる Claude のハブだけで有効にする（§3.3 の `--permission-mode default` と
  `--disallowedTools`）。ただし、ユーザー設定の allow ルールが広ければ確認なしで通りうる。これは
  強制できないので、実験機能の制約として README に書く。
- Codex のハブは起動時の設定（コマンド、モード、設定）をユーザーの agent_servers 設定から継承し
  （codex_acp_view.rs:774-796）、承認方式を強制できない。フェーズ 0 で確かめるまで限定自動を選べない
  ようにする（§3.2）。
- 設計の正本にある「限定自動では他のエージェントには広がらない」は、この範囲の保証として読む。
  ユーザーと設計担当のセッションに報告する。

### 3.7 人間の書きかけとの衝突回避（両モード）

送るかどうかの判定を、coordinator_tick_task の「ハブが idle か ready」の判定と
`waiting_since = None`（:1027-1030）の直後に置く。保留のときは error を立てずに return し、次の tick
（2 秒後）で再判定する。起動待ちの 60 秒（:937-944）はこれより前に判定されるので、優先関係は今のまま。

- Claude TUI のハブ:
  - `render_coordinator_panel` の外側の div に `.capture_key_down` を付け、`Runtime.last_key_at` を記録する
    （前例 arena.rs:1425）。
  - 保留の条件: IME の変換中（`TerminalView` に `pub fn has_marked_text(&self) -> bool` を足す。
    terminal_view/src/terminal_view.rs:428-437 の `ime_state`）、最後のキー入力から 10 秒以内、または
    ハブにフォーカスがありウィンドウが前面にある間は 30 秒以内。
- Codex ACP のハブ:
  - agent_ui に `pub enum ExternalMessageReadiness { Ready, Wait(String), Fatal(String) }` と
    `ConversationView::external_message_readiness(&self, cx: &App)` を足す。`ThreadView` には同じ判定を
    返す `pub(super)` メソッドを足し、既存の `check_external_message` はそれを使って Err に変換する
    （判定の重複を避ける。`insert_external_message` のシグネチャは変えない）。
  - Wait: 接続中・復元中、権限確認や質問への回答待ち（子スレッドを含む）、elicitation、実行中、
    読み込み中、キューあり、書きかけ、編集中。
  - Fatal: 認証が必要、会話のエラーが残っている。
  - `CodexAcpView::external_message_readiness` は、自分の状態 Error を Fatal、Running と Permission を Wait に
    してから、会話の判定を返す。
  - tick: Wait なら return、Fatal なら `runtime.error`。送信が失敗したら判定し直し、Wait なら待つ。

### 3.8 `agentium hub answer`

```
agentium hub answer --question <id> (--choice <n> [--note <text>] | --text <text>)
```

`--choice` は 1 始まり（`--recommended` と同じ）。JSON では 0 始まりの `option` に直して送る。

- 呼び出し元がそのタスクの Coordinator で、モードが限定自動であること。
- 質問の検証とエラーを分ける。`InboxQuestion::answer` を `Result` を返す形に変え、UI 側は今の挙動
  （何もしない）を保つ。
  - 無い id: 「no question #<id>」
  - 回答済み: 「question #<id> is already answered」
  - `hub_task` が呼び出し元のタスクでない: 「question #<id> belongs to another task」
  - 選択肢があるのに `--text`、選択肢が無いのに `--choice`、番号が範囲外
  - 質問者の asker が無い（Codex TUI、hook の無い Claude）
- 回答の送信は、人間の回答と同じ関数を通す（`answer_inbox_question` を、回答者を引数に取る
  `deliver_question_answer(question_id, option, note, answerer, window, cx) -> Result<()>` に切り出す）。
  - 送り先の確認（人間の回答にも効かせる）: 質問の生きているタブについて
    `inbox_asker(タブ) == question.asker` を確かめる。違えば `tab_for_asker(question.asker)` で探し直し、
    見つからなければ「the asking session is no longer open」。同じ端末で別のセッションが起動していても、
    前のセッション宛ての回答を届けないため。asker の無い質問（人間の回答だけが対象）は今どおりタブに送る。
  - 回答者が Coordinator のとき、送信の 1 行目は `[from: Task Coordinator]`、`resolution` は
    `Answered by Task Coordinator: …`（assume の追認も同じ接頭辞）。
  - 送り先の状態（idle / ready）は条件にしない（人間の回答とそろえる）。
  - 送信に失敗したら未回答のまま `question.error` に残し、ハブにエラーを返す。
  - 成功したらその質問のメモ欄（`inbox_note_editors`）を消す。人間の書きかけは捨てられる（仕様）。
- 端末の同期: `send_message_to_item` で、貼り付けの前に `terminal.update(cx, |terminal, cx| terminal.sync(window, cx))`
  を呼ぶ。非表示の質問者や、人間の回答・Go の送り先でも bracketed paste が正しく効くようにするため。
  Coordinator 側の同期（task_coordinator.rs:1042-1049）はこれに統合する。
- 連続回答の上限: 質問者（owner）ごとに、ハブが連続して答えた回数を `Runtime.answers_in_a_row` に
  `(count, baseline_turn: Option<String>)` として持つ。
  - 回答に成功したとき（同じフォアグラウンドの更新で）: count を 1 増やし、`baseline_turn` をその時点の
    owner の `Source.turn` にする。Source が取れなければ None にする（保存済みの古いジョブを今の
    ターンとして代用しない）。
  - tick で owner の今の `Source.turn` を見る。`baseline_turn` が None なら、見えたターンを baseline に
    するだけで数え直さない。Some で、今のターンが baseline と違い、依頼が空でなく
    `[from: Task Coordinator]` で始まらないときだけ、count を 0 にする。`[from: Task Coordinator]` で
    始まるときは baseline だけ更新する。依頼の本文が取れない場合は何もしない（安全側）。回答直後の
    tick は baseline と同じターンを見るので、古い人間のターンで数え直すことはない。
  - 3 回に達したら「answer limit reached; leave this question to the user」で拒否する。
  - 貼り付けた文面が UserPromptSubmit の prompt に届くかはフェーズ 0 #5 で確認する。届かなければ数え直しは
    起きず、上限に達した後は人間が答える。

### 3.9 `todo assign` の制限（全呼び出し元）

- `AssignTarget::Tab` で、送り先のタブのアリーナが項目のタスクに属することを確かめる。項目の
  `task_id` が Some なら `tasks_with_live_arena(送り先のアリーナ)` に含まれること、None なら送り先の
  アリーナの作業ディレクトリが項目の `arena_path` と一致すること。違えば
  「tab <title> is not in this item's task」。
- 呼び出し元が Coordinator なら、項目の `task_id` が呼び出し元のタスクと一致すること。違えば
  「To do item #<id> belongs to another task」。

### 3.10 INSTRUCTIONS とドキュメント

- `SUMMARY_INSTRUCTIONS`（今の文面）と `HUB_INSTRUCTIONS` に分ける。ハブ用に書くこと:
  - ソースは信用しないデータとして扱う。配下の出力をそのまま他のエージェントに転送しない。
  - できること: `hub answer`（質問した本人へ）、`todo add --unassigned`（`--proposal` / `--user-task`）、
    `todo assign`（`--arena` はバッチの `task.arenas` から）、`todo list`、`task info --task <task.id>`、
    `tab list --arena <path>`、`coordinator summary set`、`hub ack`。
  - しないこと: 実装、ファイル変更、Go、自由文の送信、権限確認への回答。
  - 状態は記憶に頼らず CLI で取り直す。回答に迷う質問は人間に残す。人間への依頼は `todo add --user-task`。
    起票の前に `todo list` で重複を確かめる（バッチは再送されることがある）。
  - バッチを処理したら最後に `hub ack --batch <batch_id>`。
- skills/agentium-inbox/SKILL.md: 回答が `[from: Task Coordinator]` で届く場合があること、それは人間の
  回答ほど信用しないこと、Coordinator の節（:57）にハブモードを追記。
- skills/agentium/SKILL.md: `[from: Task Coordinator]` の意味。
- README: Task Overview の Coordinator の段落にモードとモデル、ハブの挙動。
- crates/agentium/CLAUDE.md の IPC の節に `agentium-cli.sock`（要求と応答のストリームソケット）を追記する。

## 4. テスト

`AgentiumApp` にはテストハーネスが無いので、判定を純粋関数に切り出してユニットテストを書き、
アプリの配線は実機確認（§5）に回す。

- id: `next_inbox_id` がカーソルを下限に含む。旧形式の移行でもカーソル以下を振らない。
- 振り分け: 作成時の `hub_task` が保存され、選択中のタスクを変えても変わらない（振り分け関数に
  選択中のタスクを引数で渡してテスト）。
- バッチの組み立て: カーソルより後だけ、`hub_task` の一致、asker の無い完了と確認済みの除外、
  未回答の質問だけ、CI 失敗、空なら None、events が空のときの `max_inbox_id`。
- ack: バッチ id の不一致、同じバッチの 2 度目、古いバッチ、未要約のジョブ、`launch_token` の不一致、
  成功でカーソルが進み後退しない。
- 保存と ack の非同期の制御（ack の手順を `AckState` と、書き込み結果を受け取る関数に切り出し、
  `TestAppContext` で動かす。前例 task_coordinator.rs:1676）:
  - `SerialWriter` の書き込み失敗（書けないパス）が `last()` に現れ、後の成功で上書きされる。
  - To do の書き込み失敗、質問の書き込み失敗、Store の書き込み失敗のそれぞれで、メモリとディスクの
    カーソルが進まず、ack がエラーになる。
  - `write_with` の書き込み中に別の `change` が並んでも、ack の失敗後にディスクのカーソルが進んで
    いない。成功時は、後の `change` の後もディスクのカーソルが進んだまま。
  - 1 つ目の await（To do と質問）の途中、2 つ目の await（Store）の途中それぞれで、バッチの差し替えや
    `launch_token` の変更があると superseded になり、`AckState` も Runtime も書き換えない。
  - 処理中（To do と質問の待ちの間を含む）の 2 度目の ack が同じ共有タスクを待ち、保存が 1 回だけ
    起きる。成功後の 2 度目は成功。
  - 失敗の後に Retry してもカーソルが進んでいない。再起動（Store を読み直す）でも同じ。
  - To do・質問の書き込み失敗で tick が配送を止め、Retry の書き直しが成功すると再開する。
- hook 通信の照合: Coordinator が自分以外の `session_id` で `coordinator_claude_turn` や
  `permission_request` を送ると、状態を変えずに拒否される。
- 許可リストに、Coordinator 自身の hook 通信（`coordinator_claude_turn`、`permission_request`）が
  通ることを足す。呼び出し元の判定が、要求の自己申告の `ancestor_pids` を使わないこと（判定関数が
  peer の pid だけを引数に取る形にして、型で保証する）。
- タブが見つからない記録も、表示項目を null にしてバッチに入ること。
- 連続回答: 3 回で拒否、人間のターンで数え直し、回答直後の古いターンでは数え直さない、
  `[from: Task Coordinator]` のターンと本文の無いターンでは数え直さない。
- `InboxQuestion::answer` のエラーの種類、assume の扱い（今のテストを保つ）、回答先のセッション照合。
- 許可リスト: Coordinator の要求の種類ごとの可否、Unknown と Tab は今どおり。
- `todo assign` の送り先と、Coordinator の項目のタスク。
- 書きかけの保留の条件（時刻、フォーカス、IME）。
- readiness: 既存の ConversationView のテスト（agent_ui/src/conversation_view.rs:4101 付近）と
  CodexAcpView のテスト（codex_acp_view.rs:1988 付近）を広げ、書きかけ、子スレッドの権限待ち、
  elicitation、認証、復元中を確かめる。
- 要約のみモードの回帰: 送信の判定関数を切り出し、毎回の全文指示、1 ジョブ、300 秒、要約の登録か
  差し替えで完了、ack 不要を固定する。
- 既存の v1 変換・永続化テストに、mode、acked_inbox_id、新しい Provider の既定値と往復を足す。
- CLI の引数（main.rs の既存の `Args::try_parse_from` テストに倣う）。

## 5. 実機確認（フェーズ 0 とあわせて、ユーザー）

フェーズ 0 の項目に加え:

1. 限定自動に切り替え、配下に `todo ask` させて、ハブが `hub answer` で答え、`hub ack` まで進むか。
2. ハブに打ちかけている間、バッチが保留されるか（Claude、Codex）。IME の変換中も。
3. 人間が先に回答した質問に `hub answer` すると「already answered」になるか。
4. 再起動の前後で、バッチが重複しても欠けないか。
5. Coordinator から `tab send-message`、`tab new` が拒否されるか。
6. 非表示の質問者への複数行の回答が、途中で送信されずに 1 通で届くか。
7. Codex のハブが、許可されていないコマンドを実行しようとしたとき、承認を求めるか（限定自動を
   Codex に開けるかの判断材料）。
8. 選択中のタスクを切り替えても、共有アリーナの質問が最初のハブにだけ届くか。
9. Claude のハブで `--permission-mode default` と `--disallowedTools` が、ユーザー設定の既定モードや
   allow ルールより優先されるか。

## 6. 実装の順序

各段で clippy（agentium と、触った terminal_view / agent_ui）とテストを通す。

1. Inbox id の修正、`Coordinator` と Inbox 記録の項目追加（mode、acked_inbox_id、Provider、hub_task）。
2. 呼び出し元の特定、許可リスト、祖先探索の上限、`todo assign` の制限、送信前の端末同期。
3. モードとモデルの UI、起動引数、セッションの作り直し。
4. `SerialWriter` と Store の書き込み結果、振り分け、バッチ、`hub ack`、イベントだけでの起動、タイムアウト。
5. `hub answer`（送り先のセッション照合を含む）。
6. 書きかけの保留（terminal_view と agent_ui の追加を含む）。
7. INSTRUCTIONS、スキル、README、CLAUDE.md。
