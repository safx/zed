# Agentium Codex ACPタブ 第2フェーズ開発計画

承認済みRev 2・2026-10-13。Claudeがmust-fix 1〜3の解決と新規must-fixなしを確認し、実装を承認した。

基準は `experiment/codex-acp-tabs` の `c8d6decbb8`。調査開始時の作業ツリーは clean。[前計画](codex-acp-tab-plan.md) §6 のうち、セッションIDによる履歴復元、CLI連携、ツールカードの初期折り畳みを扱う。Claudeが must-fix のみを固定番号でレビューし、計画承認後に実装する。実装も同じ手順でレビューして収束させる。

## 1 目的と完成条件

Codex ACPタブで進めていた会話を、Agentiumを終了しても同じworktreeで再び開けるようにする。CLIからACPタブを作成・識別・送信できるようにし、復元した会話もツールカードを折り畳んで表示する。

| 対象 | 完成条件 |
| --- | --- |
| A 履歴復元 | Quit時に開いていたACPタブを、そのworktreeのArenaを次に開いたとき、保存したセッションIDで履歴付きで復元できる |
| A 終了と失敗 | Cmd+W・Arena closeでは復元対象から外す。復元失敗をタブ内に表示し、再試行またはタブを閉じる操作ができる。生成中のQuit後もクラッシュしない |
| B CLI | `tab new --type codex-acp`、`tab send-message`、`tab list`、`tab self`がACPタブを扱う。エージェントによる会話タイトル変更でCLIの宛先が変わらない |
| B 送信制約 | 承認待ち・実行中・接続中・復元中・既存下書きありのACPタブへの送信を、内容を変更する前に拒否する |
| C 初期表示 | Agentiumの既定設定で、新規会話と復元会話の編集・端末カードを折り畳む。承認に必要な内容は表示する |

## 2 スコープと確認した前提

対象はローカルのCodex ACPタブのみ。保存するのは再接続に必要なタブ情報であり、会話本文・ツール出力はアダプターが管理する履歴を使う。新規作成・復元とも1タブに1接続・1ルートセッションという所有関係を維持する。

次の機能は非スコープとする。

- Registry・NodeRuntimeによる導入と更新、他エージェント、リモートArena、汎用設定画面。
- 汎用セッション一覧ブラウザ、`list_sessions`による全履歴探索、セッションのfork・delete。
- Arena一覧、端末・エディタ等のタブ、Pane配置、選択位置、ツールカードの手動展開状態の復元。
- ACPの`tab log`、実行中メッセージのキュー投入、ACPのCLIによるPane分割。
- Quit時のACP終了方式の変更、強制終了された実行やツールの自動再開。

| 確認箇所 | 現在の動作と設計への影響 |
| --- | --- |
| `main.rs` の `initial_workspace_path` とウィンドウ作成 | パスは `arena new <path>` のときだけ得る。新規プロセスではProjectにworktreeを登録するが、Arena作成には渡していない。`AgentiumApp::new` は空の `arenas` で始まる |
| `agentium.rs` の `add_arena_with_path` / `add_arena_inner` | 起動済みアプリへのIPCやサイドバー操作からArenaを作る共通経路。`WorkspaceDb::save_local_workspace_paths` は呼ぶが、Agentiumでそれを読み戻してArena・タブを復元する経路はない |
| `arena.rs` | タブの永続化はない。新しいArenaは非同期に初期端末を作る |
| `ThreadMetadataStore` | Agentiumは初期化していない。初期化しても「どのArenaにどのタブが開いていたか」は別に保存する必要がある |
| `ConversationView::initial_state` | 保存IDがあれば `supports_load_session` を先に調べ、履歴付きの `load_session` を呼ぶ。次の候補が履歴なしのresume |
| アダプターの能力 | 依頼元がmise shimの `codex-acp 2.2.2` で確認したinitialize応答は `protocolVersion: 1`、`loadSession: true`、sessionCapabilitiesはresume/list/close/delete/fork。本計画ではこの確認を前提とし、実装検証で導入版も記録する |
| `AcpConnection::open_or_create_session` | RPC中にthreadを登録して再生通知を受ける。現状、失敗時はforegroundキューをdrainするが、成功時は同じdrainを行っていない |
| `ConversationView::set_server_state` | loadが返り、ThreadViewを作った後に `RootThreadUpdated` を発行する。ラッパーのthread購読開始はこの時点 |
| CLIの型 | clap側は `main.rs::PaneContentType`。Arenaが使う型の定義は実際には `agentium.rs::PaneContentType` で、`arena.rs` がimportしている。後者だけにFileBrowserがある |
| CLIのWindow | `handle_cli_request` 自体はWindowを持つ。現在は `handle_tab_send_message` に渡していない |

## 3 設計

### 3.1 A：保存先と復元のトリガ

`paths::data_dir().join("codex_acp_tabs.json")` に専用のJSONを保存する。`board.json` と同じく稼働中のAgentiumだけが書く。ThreadMetadataStore、AgentPanel、Native ThreadStoreは初期化しない。保存処理は新しい論理コンポーネント `crates/agentium/src/codex_acp_persistence.rs` にまとめ、AgentiumAppから所有する。

ファイルの形式はversionとworktreeごとの配列とする。各タブに永続UUID、固定のCLIタブ名、ACPセッションIDを保存する。worktreeのキーはcanonicalizeした絶対パスで、異なるworktreeの記録を混ぜない。タブUUIDはGPUIのEntityIdと分離する。配列順は作成順とし、Pane位置の保存には使わない。設定・認証情報・本文・プロセスPIDは保存しない。

初回読み込みはアプリがArena作成要求を処理する前に完了させる。ファイル不存在は空として扱う。読めないファイル、破損JSON、未対応versionはログとアプリ内の警告で知らせ、既存ファイルを空のデータで上書きしない。その起動中は永続化を停止し、通常の新規タブは利用可能にする。個々の復元失敗はファイル全体の破損として扱わない。

復元のトリガは **そのworktreeのArenaが開かれたとき** とする。

1. `add_arena_with_path` でcanonicalizeしたパスを使い、すでに同じworktreeのArenaが開いていれば既存Arenaへ切り替える。二重のArenaを作って同じセッションを重複ロードしない。
2. `add_arena_inner` でそのパスの保存記録を一度取得し、新しいArenaの初期Paneへ保存順にACPタブを同期追加する。各タブの接続と履歴ロードは非同期で進める。
3. 復元記録の有無にかかわらず、初期端末は従来どおり1つ非同期に作成・追加する。端末が後からアクティブになってもよい。復元するACPタブを追加したことを理由に、Arenaの初期端末を省かない。
4. パス指定の初回起動も、ウィンドウとAgentiumAppの作成後に `add_arena_with_path` を一度呼ぶ。現状の「Projectのworktreeだけ作る」経路をこの共通経路へ接続する。パス指定のない起動ではArenaを自動生成しない。

保存ファイルを読んだからといって過去の全Arenaを開くことはしない。ユーザーがサイドバーや `arena new <path>` でworktreeを開いたときに、そのworktreeの記録だけ使う。閉じたArenaを次に開いたときは、後述の削除方針により新しい会話となる。

### 3.2 A：保存タイミングと終了操作

新規タブは `RootThreadUpdated` からルートthreadのセッションIDを得た時点で保存対象に加える。接続失敗や認証待ちでまだIDがない新規タブは保存しない。復元対象のIDは復元開始前から保持し、接続失敗・認証待ち・再試行中に記録から消さない。ルートIDが変わった場合は同じタブUUIDの記録を更新する。

| 操作 | 保存記録と接続の扱い |
| --- | --- |
| ID取得・固定名確定 | 対象タブの記録を更新して保存する。エージェントによる会話タイトル変更では書かない |
| タブ・Arenaの切替 | 保存対象を維持し、復元も再起動もしない |
| 同一ArenaのPane間移動 | 同じUUIDとIDを維持する。移動途中のRemovedItemだけで記録を削除しない |
| Cmd+W、閉じるボタン、タブ群を閉じる | Arena内の全Paneから実際に外れたACPタブの記録を削除し、従来どおり接続を解放する |
| Arena close | `remove_arena` で対象worktreeの記録を明示削除する。閉じる操作が取り消された場合は削除しない |
| Quit・アプリ再起動 | 開いているタブの記録を保持する。接続は既存の終了ガードで終了する |
| プロセスクラッシュ・OSによる強制終了 | 最後に保存が完了した記録を次回の復元候補にする |

PaneのAddItem/RemovedItemを受けた後、`defer` でそのArenaの全Paneの所属を再確認する。移動完了後にも存在しないUUIDだけ削除する。記録から削除する条件はPaneへの所属であり、差分ビュー等がthreadを保持しているかどうかには依存させない。遅れたID取得通知が閉じたタブの記録を作り直さないよう、所属とUUIDを検査する。Dropだけを永続記録削除のトリガにしない。

書き込みはメモリ内の全記録をスナップショットにし、`persist_board` と同様に前の書き込みTaskをawaitする1本の列として処理する。同じディレクトリの一時ファイルへ書いてrenameする。タブごとの並列書き込みや、Taskを置き換えて旧書き込みをキャンセルする方式は使わない。保存エラーはログに加えて対象Arenaまたはアプリの警告に出し、次の更新で再試行できるようメモリ内の記録を保持する。

永続化のQuit observerは、windows解放より前に生存Arenaの所属を最後に照合し、まだdeferされているcloseの削除も確定する。その後に終了フラグを立て、所属確認・Dropでは記録を削除しないようにする。最新の保存Taskをobserverから返し、GPUIの終了猶予内で待つ。これはローカルの小さなJSONの保存であり、ACPの応答を待たない。Quitの猶予切れや強制終了では最後の保存分までとなることを明記する。通常終了で最後の閉じる操作まで反映されるケースをテストする。

### 3.3 A：load_session、再生と状態

`CodexAcpView` の作成引数に保存済みタブ情報を渡せるようにし、保存IDを `ConversationView::new` の `resume_session_id` に渡す。work_dirsは必ず保存記録に対応するArenaのディレクトリにする。固定のCLI名は会話タイトルの引数には流用しない。

確認済み2.2.2では `ConversationView::initial_state` のload分岐をそのまま使う。履歴の探索や `list_sessions` は不要。将来のアダプターでload非対応となった場合、既存UIのresume分岐は履歴なしとなるので、その場合は履歴なしである旨の既存表示を保ち、履歴復元成功とは数えない。能力による差を実装の検証記録に残す。

codex-acp 2.2.2はprotocolVersion 1を使い、`uses_reported_activity()` はfalseとなる。`AcpThread::status()` は `activity.phase()` に由来し、会話履歴のNewEntry/EntryUpdated/TitleUpdatedではRunningにならない。load成功後のthreadはIdleであり、末尾にInProgressのツールコールがあってもGeneratingとは扱わない。

ラッパーはload完了後にthreadの購読を開始する。現在の `handle_thread_event` は、同じ世代のStoppedを受け、状態がIdle以外ならCompletedにできるため、古いthreadや過去の実行から遅れて届くイベントについてrootと世代を検査する。v1の通常の履歴再生でStatusChanged/Stoppedが発生するという前提は置かない。

次の条件で履歴再生と今回の実行を分ける。

- `AcpConnection::open_or_create_session` の成功経路でも既存の `drain_foreground_queue` をawaitし、受信済みの再生通知のモデルへの適用を終えてからthreadを返す。drain失敗はロード失敗として伝播し、pending sessionの後始末も既存の失敗経路に揃える。
- ラッパーはrootのEntityIdとロード完了時のactivity_generationを基準として購読し、初期状態をIdleにする。新しい世代の実行開始を観測したときだけ、その世代を完了判定の対象にする。基準世代のStoppedや、旧thread・再接続前の通知ではCompletedにしない。
- 成功扱いにするのは、その実行世代でRunningまたはPermissionを観測し、正常停止を受けた場合だけ。CancelledはIdle、Error/LoadError/RefusalはErrorとする。ロード失敗を正常停止で上書きしない。
- SIGKILL後の履歴末尾にInProgress・未応答のツールコールが残っていても、ロード成功後はIdleのまま入力・CLI送信を受け付ける。その履歴だけでRunning・Permission・Completedへ遷移させず、エラー扱いにもせず、ツールを自動再実行しない。新たな承認要求がある場合は既存UIと送信制約を適用する。load直後のGeneratingを検査する分岐は今回追加しない。
- 再試行開始時に旧購読と完了対象世代を無効化し、新rootの購読時に取り直す。再生中はCLI送信を受け付けない。

共有接続の成功時drain追加はZedにも作用するため、再生通知が応答直前にキューへ入るテストと既存load/closeのテストで順序と解放を確認する。通常の新規会話・次のターンの完了バッジも回帰検証する。

### 3.4 A：復元失敗のUIとQuit中断

復元中はタブの固定名を表示し、会話領域に「会話を復元中」を表示する。失敗時もタブを残し、何の履歴を開こうとして失敗したか分かるようにする。

| 失敗 | 表示と操作 |
| --- | --- |
| codex-acp不在・起動失敗 | 既存の導入案内、設定パス、LoadError、再試行を使う。保存IDは保持する |
| 認証が必要 | 既存の認証UIを使い、認証後も保存IDと同じcwdでロードする |
| セッション消失・load失敗 | 「保存した会話を復元できませんでした」と原因を表示する。再試行は同じIDを使用する |
| 復元後の接続断・LoadError | Error状態と既存のエラーUIを表示し、送信を拒否する。再接続で同じ履歴を再ロードする |
| 保存ファイルの読み書き失敗 | 会話の通信エラーとは別に、復元情報を保存できていないことと実パスを表示する |

失敗したIDから自動で新規セッションへ切り替えない。新しい会話は既存の＋メニューまたはCLIで別タブとして開ける。失敗タブを閉じればその復元記録を忘れる。この操作はCodexが保存した会話履歴のdeleteを意味しない。

Quit時は現在の `killpg(SIGKILL)` がadapterと子プロセスを終了する。生成中の会話は、最後までディスクに書かれた履歴だけが戻る場合、途中まで戻る場合、ロードが失敗する場合を許容する。自動でプロンプトを再送しない。ロード成功時は、末尾にInProgress・未応答のツールコールが残ってもIdleで新しい入力・CLI送信ができることを要求する。ロード自体が失敗した場合は原因を表示し、再試行・閉じる操作を可能にする。クラッシュ、永久に送信中の表示、未送信なのにCompletedになる状態は不可とする。

### 3.5 B：CLIからの作成と安定した名前

`main.rs` のclap型と `agentium.rs` の実行用 `PaneContentType` の両方に `CodexAcp` を追加する。文字列は `codex-acp`。`run_tab_action` の変換、`parse_content_type`、Arenaの `add_tab` の分岐を接続する。FileBrowserのCLI公開や型の統合は行わない。

`tab new --type codex-acp --title Codex-Review` は現在選択中のPaneに、新しい独立したACPセッションを作る。`--` 以降の端末用commandはACPではエラーにする。作成成功の応答はタブを作ったことを意味し、認証・接続完了はタブ内で確認する。

このフェーズでは、非表示Arenaに対する `tab new --type codex-acp` は既存の非端末タブと同じく明示エラーとする。勝手にArenaを切り替えない。既存ACPタブへの送信・一覧表示は非表示Arenaでも利用できる。

両PaneContentTypeは `pane split` でも参照される。ACPのsplitは今回の対象外なので、CLIで `--type codex-acp` を指定した場合はIPC送信前にエラーにする。サーバーのpane split経路とArena側の全matchでも明示的に拒否し、未知の型としてTerminalにフォールバックしたり、成功したように見える無処理にしたりしない。

ACPタブに固定の `cli_title` を持たせる。`--title` はこの値を設定し、未指定なら同じArenaで未使用の `Codex ACP`、`Codex ACP 2`、…を採番する。空文字・空白のみ・改行を含む名前は拒否する。明示名が他のタブと一致した場合も新規作成を拒否する。復元では保存した名前をそのまま使う。

`Item::tab_content_text`、`tab list`、`tab self`、`--title` の完全一致検索はこの固定名に揃える。動的な会話タイトルは会話UIとタブtooltipに表示する。会話のタイトル変更・再接続・復元でも固定名を変えない。後から端末の動的タイトルが同じ名前になった場合は、従来どおり一致数が複数なら送信を拒否する。

### 3.6 B：送信経路と状態

宛先検索はまずArena内の全Itemでタイトルの一致数を検査し、一意なItemをTerminalまたはCodexAcpに分類する。端末は既存の `ensure_safe_message_target`、paste、Enterを維持する。`tab log` は一意検索後に端末かを検査し、ACPには非対応のエラーを返す。

`handle_cli_request` が持つ `window` を `handle_tab_send_message` まで渡す。ACP側の入口を `CodexAcpView` に設け、対象ConversationViewの **root_thread_view** を使う。表示中の子会話への誤送信を避ける。

ACPでは以下をすべて検査してから入力欄を変更する。

1. ConversationViewが接続・認証済みでrootがあり、ロードエラーや復元失敗がない。
2. CodexAcpStateがPermissionでなく、threadの `is_waiting_for_confirmation`、子会話を含む承認待ち、接続単位のpending elicitationもない。認証UIにも送信しない。
3. Runningでなく、実際のthreadがIdle。`is_loading_contents` や送信キューがある場合も拒否する。状態バッジだけで判定しない。
4. message_editorに下書きや添付・mention等の入力コンテキストがない。既存の `is_empty` はテキストだけを検査するため、必要な「CLI投入で既存入力を巻き込まない」検査をagent_ui側に最小のAPIとして置く。

過去の履歴にあるInProgress・未応答のツールコールは、それだけでは送信拒否の条件にしない。実際のthread状態と現在の承認・elicitation要求を使って判定する。

承認・elicitation・認証状態等の非公開情報は、ConversationView/ThreadViewの送信可否を返す小さなAPIで確認する。汎用UIの状態をAgentium用に広範囲に公開しない。検査と投入は同じforeground更新内で行う。

共通の `with_from_line` で `[from: ...]` を一度だけ先頭に付け、空のmessage_editorへ公開APIの `insert_text` で投入する。`set_text` は現状test/test-support限定なので製品経路では使わない。`--submit` があれば同じThreadViewの `send(window, cx)` を呼ぶ。なければ下書きとして残す。挿入や送信のために対象Arena・タブへフォーカスを移さない。

RunningとPermissionでは `--submit` の有無にかかわらず拒否する。`send_queued_message_now` は使わない。CLI成功は入力受付・送信開始の受理までを意味し、モデル応答の成功を保証しない。以後の通信エラーは既存のThreadViewエラーUIに伝播する。連続する要求では最初のsendで立つ `is_loading_contents` も検査し、次の要求を同じ下書きに混ぜない。

| CodexAcpState | `tab list` のkind | state |
| --- | --- | --- |
| Idle | `codex-acp` | `idle` |
| Running | `codex-acp` | `running` |
| Permission | `codex-acp` | `permission` |
| Completed | `codex-acp` | `ready` |
| Error | `codex-acp` | `error` |

接続・認証・復元中は既存のIdle相当であり、`idle` だけでは送信可能とは限らない。CLIは送信時に上記の実状態を確認する。正常完了をユーザーが確認すると `ready` から `idle` になる。端末のkind/stateと、その他Itemの `other` / nullは維持する。

### 3.7 B：tab selfと送信者検証

ACPの子プロセスからCLIを実行したときは、接続が所有するadapterプロセスのPIDが祖先列に含まれるかでタブを解決する。`AcpConnection` に、現在生存しているローカルプロセスのPIDを返す読み取りAPIを追加する。`AcpProcess` の既存exitedフラグを使い、終了・明示terminate・status取得エラー後はNoneとする。未接続、fake、リモートにもNoneを返す。PIDは保存せず、再接続時は新しい接続から取得する。

CodexAcpViewから現在の接続にdowncastしてこの値を取得する。ロードエラーや再試行中に残った旧threadからPIDを採用しない。端末のshell PID取得とACPのadapter PID取得をタブ解決用の共通処理にまとめ、`tab_self` と `find_arena_entity_id_for_pids` の両方で使う。祖先に複数タブが見つかった場合は最も近い祖先を選び、同じ優先度で曖昧なら検証済みとしない。

送信者ラベルは現行規約を維持する。検証済み送信者に使うのはUnixソケットのkernel peerから取得した祖先列だけであり、要求JSONの `ancestor_pids` や `--from` は検証根拠にしない。検証済みタブ名と `--from` が異なる場合は既存の警告を返す。祖先関係を失ったdaemon経由等は従来どおり `(unverified)` とし、名前からACPタブを推測しない。

mise shim、起動シェル、Codex App Server、ツール用シェル、CLIの実プロセスで親子関係を確認する。detachedな経路では `tab self` が失敗し得るが、未検証名で送る規約は継続する。終了済みのPIDを別プロセスが再利用しても古いACPタブへ結び付けないことをテストする。

ACPの起動環境には端末用の `terminal::insert_zed_terminal_env` が適用されていないため、`codex_acp_view.rs::build_launch` で `TERM_PROGRAM=agentium` と `TERM_PROGRAM_VERSION` を設定する。versionは端末と同じ `release_channel::AppVersion::global(cx).to_string()` を呼び出し元で取得し、build_launchへ渡す。これにより `insert_zed_terminal_env` がAgentium本体で設定するホスト名とバージョンに揃える。

directory環境、proxy環境、settingsの明示envを重ねた後、この2値をホスト識別値として設定する。他のenvの優先順は維持する。directory環境を取得できなかった場合も2値を設定し、既存の継承環境と警告を使う。既存build_launchテストに、通常起動・環境取得None・上流envに別のホスト名やversionがあるケースを追加し、最終的な2値を検査する。None時にenv全体が空であるという既存の期待値も更新する。

`crates/agentium/skills/agentium/SKILL.md` を実装時に更新する。ACPのkind/state、固定名、new、送信拒否条件、下書き投入とsubmit、tab log非対応、検証済み/未検証送信者を説明する。`TERM_PROGRAM=agentium` を前提とする既存ガードは維持し、Agentium外では止まる。個人環境にインストール済みのスキルはこの変更の対象外とする。

### 3.8 C：ツールカードの初期折り畳み

`main.rs` の既存 `SettingsStore::update_default_settings` 内で `content.agent` を必要に応じて初期化し、`expand_edit_card` と `expand_terminal_card` を `Some(false)` にする。他のagent設定フィールドを置き換えない。ユーザーが明示的にtrueを設定した場合は、既存のユーザー設定優先順に従う。設定ファイルの再読込後もAgentiumの既定値はfalseのままとする。

確認した通常の自動展開経路は `ThreadView::handle_entry_view_event` のNewDiffとNewTerminalで、いずれもこの既存設定を読む。`EntryViewState::expanded_tool_calls` は空で初期化される。その他のexpand呼び出しは明示的な展開API、ユーザー操作、テストであり、Agentiumの自動展開の追加経路は見つかっていない。

`EntryViewState::is_tool_call_content_visible` の承認待ち判定は維持する。これはexpanded_tool_callsへの追加ではなく、承認中だけ内容を見せる判定なので、初期折り畳みと両立する。新しい設定項目やCodexAcpView固有の折り畳み状態は追加しない。

## 4 実装順序と変更先

計画の承認後に着手する。ソース変更前にルートREADMEの先頭が次の2行であることを確認し、なければ最初に追加する。現在のHEADには存在する。エージェントは削除しない。

```markdown
> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
```

| 順序 | 作業 | 次へ進む条件 |
| --- | --- | --- |
| 1 | 保存形式、単一writer、所属・終了判定、固定タブ名、Arenaオープン経路 | 保存と削除、同じworktreeの重複防止、Quit時の保持がfakeと一時ファイルで確認できる |
| 2 | IDを渡す復元、成功時drain、世代判定、失敗UI | fakeの履歴が戻り、再生だけではCompletedにならず、失敗から再試行できる |
| 3 | CLI型、宛先分類、Window伝播、入力検査、起動envのホスト識別、PIDによるタブ解決 | CLIによる送信と拒否、安定名、ホスト名・version、sender labelが確認できる |
| 4 | Agentiumの既定設定、スキル・READMEの操作説明 | 新規・復元のツールカードが折り畳まれ、ユーザー設定の上書きも機能する |
| 5 | 実アダプター検証、ビルド・回帰テスト、実装レビュー | 下記受け入れ条件を確認し、未実施の実機項目は明記してClaudeへレビュー依頼する |

主な変更先は `agentium/src/{main.rs,agentium.rs,arena.rs,codex_acp_view.rs,codex_acp_persistence.rs}`、`agent_servers/src/{acp.rs,acp/transport.rs}`、`agent_ui/src/{conversation_view.rs,conversation_view/thread_view.rs,message_editor.rs}`、`agentium/skills/agentium/SKILL.md` とREADME。EntryViewStateの直接検査はagent_ui内のテストかtest-support限定の検査APIで行い、製品用にHashSetを公開しない。テストに必要なfakeの拡張は既存 `agent_servers::test` にまとめる。

## 5 検証計画

GPUIテストの実装前にgpui-testスキルを読み、executorのtimerを使う。新しいコードでは `unwrap()` と `let _ =` を使わず、保存・ロード・非同期送信のエラーを伝播または表示する。既存ファイル全体を整形しない。

### 5.1 自動テスト

| 分類 | 確認内容 |
| --- | --- |
| 保存形式 | version、2つのworktree、複数ID・固定名の往復。不在・破損・未対応version・読み書き失敗。破損ファイルを空で上書きしない |
| 書き込み順序 | ID取得直後のclose、連続更新、書き込み中のArena closeで古いスナップショットが最後に戻らない。エラーがUIへ届く |
| 復元トリガ | 保存済みでも別Arenaを開くまでloadしない。対象Arenaを開くと各IDを一度だけloadする。同じpath・symlinkからの重複openは既存Arenaに合流する |
| パス指定起動 | 新規プロセスの `arena new <path>` から共通Arena作成経路を通る。指定なしでは全Arenaを自動復元しない |
| 初期端末 | 復元記録あり・なしの両方で初期端末が1つ作成される。ACPタブの同期追加後に端末が非同期で追加されても、保存IDと復元タブを維持する。端末の後からのアクティブ化は許容する |
| 終了と移動 | Cmd+W・タブ群close・Arena closeで記録を削除。Pane移動で保持。差分がthreadを保持しても閉じたタブを復元しない。遅れたID通知で復活しない |
| Quit | `cx.shutdown()` でquit observerとwindows解放を実行する。最新記録を保持し、解放後の空snapshotで上書きしない。直前のcloseは保存に反映する |
| 履歴とcwd | FakeAcpAgentServerのloadでuser/assistant/tool履歴を再生する。新規セッションを勝手に作らず、ID・process cwd・session cwdが対象Arenaと一致する |
| 再生順序 | v1のNewEntry/EntryUpdated/TitleUpdated相当の再生通知をload応答直前まで遅延させ、成功時drain後に履歴が揃い、IdleのままCompleted・完了バッジを出さない。旧Entity・旧世代・再試行前のStatusChanged/Stoppedは別途注入し、現在の状態を誤更新しないことを検査する |
| 中断したツールの履歴 | loadで末尾がInProgress・未応答のツールコールを再生する。ロード成功後はIdleでUI入力・CLI送信ができ、履歴だけではCompletedにならず、クラッシュしない。未完了の履歴を現在の承認要求と取り違えない |
| 次の送信 | 復元後に送ったターンではRunning→Completed、キャンセルでIdle、失敗でErrorとなる |
| 復元失敗 | 不在コマンド、セッション消失、部分再生後のLoadError、auth要求、接続断でUIが操作可能。同じIDの再試行、新規別タブ作成、失敗タブcloseが機能する |
| CLI作成・名前 | type変換、command拒否、非表示Arena作成拒否、split拒否、空名・重複名拒否、未指定時の採番。会話タイトル変更・再接続・復元でも宛先不変 |
| CLI送信 | rootへ正しいfrom行を1つ付けて投入。submitありはpromptが1回、なしは0回。非表示Arenaへ送ってもフォーカス不変。端末の既存安全判定が維持される |
| CLI拒否 | Permission、Running、load/auth、LoadError、pending elicitation、子会話の承認、内容解決中、キュー、下書き・添付・mentionがあるケース。拒否時に入力欄・承認回答・prompt件数を変更しない |
| CLI一覧 | Idle/Running/Permission/Completed/Errorのkind/stateと、確認済みCompleted→Idle。その他Itemのstateはnull |
| PIDと送信者 | adapter→子シェル→CLIの祖先でselfとArenaが一致。peer由来だけでverifiedにする。偽の要求ancestor_pidsではverifiedにしない。終了・再試行・PID再利用では旧タブへ一致しない |
| ホスト識別env | 既存build_launchテストでTERM_PROGRAMがagentium、TERM_PROGRAM_VERSIONが渡したAppVersionと一致することを検査。通常・directory環境None・env値の競合でも一致し、他のenvの優先順と取得失敗時の警告を維持する |
| 初期折り畳み | Agentiumと同じ既定設定を適用し、新規のNewDiff/NewTerminalとload_sessionで再生された両種類のカードを実際に生成する。レイアウト・イベント処理後もexpanded_tool_callsが空であることを検査する |
| 展開の例外 | 承認中のカード内容は見えるが展開集合には追加されない。手動展開は使える。ユーザー設定trueでは従来どおり自動展開。設定再読込後も既定falseを保持する |
| Zedへの回帰 | load/close/再試行、既定設定trueの会話UI、プロセス終了ガード、起動オプションなしの接続が従来どおり動く |

折り畳みテストは空の履歴で空集合だけを確認しない。diffとterminalの両方が表示モデルに存在し、初期化時に発火するNewDiff/NewTerminalも処理されたことを確認する。必要なテスト用の集合サイズ取得だけをtest-supportに置く。

### 5.2 実アダプターとCLI

実施時のmise shim、codex-acp、Codex App Serverのバージョンを記録する。検証用worktree・タブを使い、既存会話を閉じたり既存セッションを削除したりしない。

- 会話・編集・端末実行を含む履歴を作り、Quit後に同じArenaを開くと同じIDの履歴が戻り、ツールカードが折り畳まれることを確認する。
- **Generating中にQuit**し、adapter/App Serverの終了後に同じArenaを再度開く。ロード成功時は、末尾にInProgress・未応答のツールコールが残っていてもIdleでUI入力・CLI送信ができることを確認する。ロード自体が失敗した場合は操作可能な復元エラーを表示する。クラッシュ、生成の自動再実行、履歴再生による完了バッジは不可。
- Cmd+WとArena close後は、同じworktreeを開いてもそのタブが戻らないことを確認する。
- 未導入アダプターと存在しないセッションIDで、エラー表示・同じIDの再試行・閉じる操作を確認する。
- `tab new --type codex-acp --title <固定名>` で作成し、list、自動で変わった会話タイトルの後のsend-message、Permission/Running拒否、submitなしの下書きを確認する。
- ACPが起動したツールでTERM_PROGRAMとTERM_PROGRAM_VERSIONがAgentium端末と一致することを確認する。同じツールから `tab self` と別タブへのsend-messageを実行し、mise shimを含む祖先関係と検証済みfrom行を確認する。daemon化等で祖先が切れる場合はunverifiedであることを確認する。
- 非表示Arenaへの送信がフォーカスを変えないことと、復元記録があるArenaにも初期端末が1つ追加されることをネイティブUIで確認する。初期端末が後からアクティブになる挙動は許容する。

### 5.3 検証コマンド

[前フェーズの実施記録](codex-acp-tab-validation.md)で通過したターゲットを使う。計画レビュー中のRev 2ではビルドやテストを実行していない。実装後は次を基本とし、共有APIを変更したクレートのテストも実行する。

```sh
git diff --check
cargo check -p agentium -p agent_servers -p agent_ui
cargo check -p zed
cargo test -p agentium --lib
cargo test -p agent_servers --lib
cargo test -p agent_ui --lib conversation_view
cargo fmt -p agent_servers -p agent_ui -- --check
./script/clippy -p agentium -p agent_servers -p agent_ui
```

依存が揃っている環境では前フェーズと同じく `--offline` を使用できる。`cargo fmt -p agentium` は実行しない。fmt checkのパッケージ指定もagent_serversとagent_uiに限定する。Agentiumの変更箇所は差分で書式を確認する。CLIのclapとIPC変換はmain.rsにあるため、libテストだけでは完了とせず、agentiumのcheckとビルド済みCLIの実操作を組み合わせる。

## 6 リスクと限界

| リスク | 対処と限界 |
| --- | --- |
| 強制終了時の履歴・保存漏れ | ACP本文はadapter側の保存状況に依存する。Agentiumの記録は逐次保存し、通常Quitは最新の書き込みを待つ。強制終了や終了猶予切れでは直前の変更を失う可能性を残す |
| worktreeの移動・削除 | canonical pathが変われば自動で同一視しない。不存在パスへ別cwdで接続しない。パス移行・履歴探索は対象外 |
| 同一セッションの重複接続 | 同一pathのArena再作成を避け、復元は一度にする。保存ファイル内の重複UUID/IDも検証して二重ロードを防ぐ |
| 復元とライブイベントの混同 | 成功時のforeground drain、rootと世代の基準、次ターンのテストで防ぐ。新しいアダプターの能力・再生仕様は導入版で確認する |
| 共有UI・接続APIへの影響 | 追加APIは送信可否と生存PIDに絞る。Zedの既定設定を変えず、共有クレートとZed本体を検証する |
| PIDと祖先の限界 | 生存接続のPIDだけ使い、peer由来で送信者を検証する。detachedな実行では未検証へフォールバックする |
| 固定名への表示変更 | CLIとタブ見出しを一致させる。動的な会話タイトルは会話UIとtooltipで確認できる |
| 初期折り畳みとユーザー設定 | 変更は既定値だけなので明示trueは優先される。承認待ちの可視性も維持する |

この計画の承認後に実装へ進む。実装レビューでは同じ番号のmust-fixを追跡し、完了報告には実施コマンド、実アダプターのバージョン、実機で確認したケースと未実施ケースを記載する。

## 7 計画レビューの反映

Rev 1に対するClaudeのmust-fix 1〜3をRev 2に反映した。番号は以後も固定する。以下は反映内容の記録であり、解決の承認は再レビューで確認する。

| 番号 | 反映箇所 | 変更内容 |
| --- | --- | --- |
| 1 | §3.7、§4、§5.1、§5.2 | build_launchでTERM_PROGRAM=agentiumと端末と同じAppVersionを設定する。既存のenvテストを拡張し、SKILL.mdのTERM_PROGRAMガードを維持する |
| 2 | §3.1、§5.1、§5.2 | 復元ACPタブを同期追加し、初期端末は従来どおり非同期に1つ作る。端末の後からのアクティブ化を許容する |
| 3 | §2、§3.3、§3.4、§3.6、§5.1、§5.2 | protocolVersion 1の再生ではthreadがIdleであることを明記。到達不能なGenerating復元エラーを削除し、InProgress・未応答のツール履歴が残ってもIdleで入力・CLI送信でき、Completedにならずクラッシュしない条件へ変更する |

2026-10-13、ClaudeがRev 2を承認。must-fix 1〜3はすべて解決済み、新規must-fixは0件。
