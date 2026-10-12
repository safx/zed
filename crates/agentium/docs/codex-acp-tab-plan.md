# AgentiumにCodex ACPタブを追加する開発実行計画

確定版（Rev 3）・2026-10-11・Claude承認：必須修正0件

Agentiumスキル経由でClaudeに3回レビューを依頼し、必須修正1～9すべての解決を確認した。最終レビューの任意指摘も終了statusの取得エラー方針とQuitフックのテスト方法に反映した。

Agentiumのタブストリップの＋メニューに「Codex (ACP)」を追加する。選択すると、現在のArenaの作業ディレクトリでCodexの会話を開始し、通常のタブとして表示する。ZedのACP接続、会話モデル、会話UIを流用する。

## 1 対象と完成条件

初期版はローカルのCodexだけを対象とし、1タブにつき1つの接続プロセスと1つのルートセッションを持つ。インストール済みの現行Codex ACPアダプターを起動する。Codex CLIの既存ターミナルタブと併用できる。

| 初期版に含める機能 | 完成条件 |
| --- | --- |
| タブ作成 | ＋メニューの「Codex (ACP)」から、選択中のPaneに新規タブが開く |
| 会話 | 入力、逐次応答、ツール実行結果、ファイル変更表示、モデルとモードの選択が使える |
| 認証 | アダプターが提供するログインUIを使い、認証失敗と再試行を表示する |
| 承認と停止 | コマンド・編集の承認を回答でき、停止操作は対象タブだけに作用する |
| 作業先 | プロセスとセッションのcwdを作成元Arenaに揃える。環境取得は同じArenaの端末と同じ経路を使い、取得できなければ継承環境と警告を使う |
| タブ操作 | 切替、閉じる、同一Arena内のPane間移動で既存のタブ操作が使える |
| 状態表示 | 実行中、承認待ち、未確認の完了、エラーをタブとArenaから確認できる |
| 終了処理 | タブ・Arena・アプリ終了時に接続と関連する子プロセスを終了する |

ACP Registryからの自動インストール、アプリ再起動後の履歴復元、CLIからのACPタブ作成・メッセージ送信、Claude等の追加対応、リモートArenaは後続開発とする。既存のCLIはACPタブを`other`として列挙し、端末専用コマンドの対象にはしない。

初期版は同じ会話を複製するPane分割を提供しない。同一Arena内の移動では同じEntityを移す。現在のArena切替とPane移動では、Arenaをまたぐタブ移動に到達できないため、新しい拒否処理は追加しない。

## 2 流用する実装と制約

基準コミットは`35ebe4fe61`。実装開始時に差分を確認する。

| 実装 | 用途と制約 |
| --- | --- |
| `agentium/src/arena.rs`の`new_agentium_pane` | 通常タブの追加、移動、＋メニューを流用する |
| `agent_servers::AcpConnection` | stdio接続、初期化、セッション、クライアント側ツール処理を流用する。現状はプロセスcwdとクライアント名にZedの既定値が入る |
| `acp_thread::AcpThread` | 会話、ツール、承認、停止、実行状態を流用する |
| `agent_ui::ConversationView` | 入力欄、会話、認証、承認、ツールカードを流用する。`Item`へのラッパーが必要 |
| `agent_ui::AgentConnectionStore` | 接続状態と再試行を流用する。同じAgentを共有するキャッシュなので、初期版ではタブごとに作る |
| `project::ProjectEnvironment` | 対象ディレクトリの環境を取得する。`default_environment`は共有Projectの既定worktreeを選ぶため使わない |
| `ConversationView`の`ThreadViewItem`テスト | 会話UIをPaneのItemに包む既存例として使う |

Agentiumは全Arenaで1つの`Project`と内部`Workspace`を共有する。`Workspace`は描画ツリーに存在せず、Workspaceに登録したアクションだけでは操作を受け取れない。既存の通知表示判定もAgent Panelを前提にしている。これらをタブ用のホスト処理で接続する。

現行のアダプターは`@agentclientprotocol/codex-acp`。ACPの要求をCodex App Serverに変換する。旧`@zed-industries/codex-acp`は新規導入の対象にしない。Agentiumの`NodeRuntime`は無効なので、初期版では既存のNode管理やRegistryを有効化しない。

## 3 設計

### 3.1 タブと接続の所有関係

新しい論理コンポーネントとして`crates/agentium/src/codex_acp_view.rs`を1本追加する。`CodexAcpView`、Codex用の起動処理、状態変換、テストをここに置く。

`CodexAcpView`は`Item`、`Render`、`Focusable`を実装し、`Entity<ConversationView>`とタブ専用の`Entity<AgentConnectionStore>`を保持する。タブID、作成元Arena、作業ディレクトリ、worktree IDを保持し、タブ移動で接続やセッションを作り直さない。相互参照にはWeakEntityを使う。

表示と入力は`ConversationView`に委譲する。会話タイトルの変更をタブへ通知し、接続前は「Codex (ACP)」を表示する。タブ切替やArena切替でTaskをdropしない。設定変更は既存の会話を自動再起動せず、再接続時に新しい起動設定を使う。

### 3.2 アダプター設定と起動

Agentium自身のsettings.jsonにある`agent_servers.codex-acp`設定を使い、新しい設定体系は作らない。`paths::set_app_name("Agentium")`によりZedの設定ファイルとは別になる。通常のmacOS環境では`~/.config/agentium/settings.json`で、画面の導入案内には`paths::settings_file()`が返す実際のパスを表示する。初期版では`type: custom`だけを起動できる。設定例は次のとおり。

```json
{
  "agent_servers": {
    "codex-acp": {
      "type": "custom",
      "command": "/absolute/path/to/codex-acp",
      "args": [],
      "env": {}
    }
  }
}
```

設定がない場合はbare nameの`codex-acp`を渡し、既存の`ShellBuilder::new(&Shell::System).non_interactive()`による起動シェルに解決させる。Agentium側で別のwhich処理は追加しない。コマンド未導入はspawnエラーではなく、シェル経由の`LoadError::Exited { status: 127, stderr }`として届く。この場合はタブ内に設定例、現行パッケージの導入案内、再試行を表示する。他の起動失敗でも原因と再試行を表示する。＋メニューは残す。Agentiumの設定にRegistry形式がある場合は、初期版がcustom起動を必要とすることを表示し、設定を自動で上書きしない。

Codex用の`AgentServer::connect`が設定または既定コマンドから`AgentServerCommand`を自前で作り、公開されている`AcpConnection::stdio`の起動オプション付きAPIを直接呼ぶ。未設定のエージェントは`AgentServerStore`に登録されないため、接続時に`get_external_agent`や`CustomAgentServer::connect`へは委譲しない。設定の読み取り、既定モード、設定選択は既存の処理を流用する。共有Projectの`get_command`で別worktreeの既定環境を生成してから部分的に上書きする方式は使わない。

設定の保存ではcustomのcommand・args・envを保持する。設定なしのPATH起動からモデルやモードを保存する場合も、同じ起動コマンドを持つcustom設定として作る。`CustomAgentServer`の既定保存処理は未設定時にRegistry設定を作るため、そのまま委譲しない。保存後の新規タブでも同じアダプターを起動できることを検証する。

環境取得は既存端末と同じ`ProjectEnvironment::local_directory_environment(&Shell::System, cwd)`を使う。worktreeへの登録は起動の前提にしない。返り値は`Option`なので、`Some`は取得環境を使い、`None`は継承環境で起動して「ディレクトリの環境を取得できなかった」旨をタブに表示する。APIから得られない詳細エラーを捏造しない。GUI起動時は、gh/beeのprobeだけでなくACP起動もログインシェル環境の取得完了シグナルを待つ。既存のoneshot ReceiverをShared化し、probeと複数のACPタブが同じ完了結果を待てるようにする。

環境は既存の継承環境に、取得できたディレクトリ環境、プロキシ設定、明示された`env`の順で重ねる。transportの`envs()`を使い、新しい`env_clear`経路は作らない。`HOME`や`CODEX_HOME`は作業ディレクトリ用途に置き換えない。これは作業先選択の整合性を保つ設計であり、Arena間の環境やファイルアクセスを完全に隔離する仕組みではない。

プロセス起動には作成元Arenaのcwdと環境の上書きを渡す。セッションには同じディレクトリだけからなる`PathList`を渡し、共有Projectの全worktreeを渡さない。作業ディレクトリが未設定または消失していればタブ内に起動エラーを表示し、誤った既定ディレクトリへフォールバックしない。

`AcpConnection`に、cwdとクライアント名・バージョンを指定する起動オプション付きAPIを追加する。Agentiumは`connect_client_future`の名前とACP initializeの`Implementation::new`の名前を両方`agentium`にする。既存の`stdio`とZedの呼び出しは既定値に委譲し、既存動作を維持する。初期版はローカル起動だけでこのオプションを使う。

認証はアダプターのACP認証方式を使う。独自OAuthや認証ファイルの複製は行わない。`CODEX_API_KEY`と`OPENAI_API_KEY`は既存環境または明示設定から渡す。承認・sandboxモードはアダプターが提示する設定に従い、自動で権限を拡大しない。

### 3.3 会話UIの初期化とホスト接続

Agentiumの既存初期化に`language_model::init(cx)`を追加する。`LanguageModelRegistry`は会話の認証・rate limit等のエラー表示でも参照される。`AgentSettings`はinventoryで自動登録され、テーマとフォントは既存の`theme_settings::init`、draft prompt保存用の`AppDatabase`は既存の起動処理で初期化済みなので流用する。`language_model`をAgentiumの依存に加える。

`agent_ui::init`、Native ThreadStore、Agent Panel、inline assistantは初期化しない。`ThreadMetadataStore`は`try_global`で省略できるため初期化しない。会話は`thread_store: None`、接続キー`Agent::Custom { id: "codex-acp" }`で作る。アプリ再起動後の履歴復元は提供せず、既存draft保存が使うDBはAgentiumのデータ領域になる。

通常のファイルリンク、ツール位置、差分Pane表示は既存の`Workspace::open_path`、`split_path`、`add_item_to_center`を使う。これらは`last_active_center_pane`を参照し、ArenaのPane Focusイベントですでに同期されている。ファイルを開くための新しいルーティングは作らない。

ホスト連携の追加は次の2点に限定する。指定しない既存呼び出しにはZedの動作を残す。

1. 会話の表示判定と通知クリック先。タブが表示・フォーカスされていれば通知を抑え、通知をクリックしたらAgentiumAppの対象Arena・Pane・タブを選ぶ。MultiWorkspaceへのdowncastを前提にしない。
2. 内部Workspaceのactive paneへ直接追加する処理。`open_markdown_in_workspace`と`ui/mention_crease.rs`の追加先を、Agentiumでは既存の`add_item_to_center`へ接続する。初期Paneを永久に保持しない。

編集レビューはスレッドに紐づく`AgentDiffPane`と会話内の編集カードを使う。`new_agentium_pane`で各Paneのツールバーに、公開済みの`AgentDiffToolbar`を追加し、差分PaneのKeep All / Reject Allボタンを表示する。各アクションは差分Pane自身のハンドラを使う。共有Workspaceのインライン編集レビュー用アクションはArenaへ再登録しない。

エディタ内のインライン編集レビュー（`single_file_review`）は初期版で提供しない。READMEにもこの制約を明記する。

TerminalPanelやAgentPanelを必要とする選択コンテキスト、過去スレッド参照等は初期版で提供しない。関連するコントロールを隠すか利用できない理由を表示し、Noneやエラーになる経路を無反応のまま残さない。`AgentThreadSource`は既存値を使い、新しいvariantは追加しない。

最初の検証で会話、モデル・モード選択、承認、ファイルリンク、差分が成立しない場合は、共有UIの不足APIを先に追加する。独自会話UIへの全面置換は計画を再レビューしてから判断する。

### 3.4 実行状態と通知

ACPタブの状態はタブEntity IDをキーとする。端末のPID集合へ架空のPIDを入れない。`AcpThreadEvent`、`is_waiting_for_confirmation`、実行状態、エラーを使って状態を更新し、必要な箇所へ`cx.notify()`を伝える。

| 状態 | 表示と遷移 |
| --- | --- |
| 未送信・接続中・認証中 | 完了扱いにしない。接続や認証の進行はタブ内に表示する |
| 実行中 | 既存の緑の表示へ接続する |
| 承認待ち | 実行中より優先してオレンジを表示する |
| 正常終了 | 送信済みの実行が正常に終了したときだけ、未確認の完了として青を表示する |
| 停止・失敗 | 正常完了と区別する。失敗は赤とエラー表示、ユーザー停止は待機状態にする |
| 確認済み | 対象タブが実際に表示・フォーカスされたときに完了通知を解除する |

タブのインジケーター、Paneの枠、Arenaのバッジとバッジからの移動を接続する。集計は端末とACPタブの状態をそれぞれ取得して合算する。初期化時のIdleだけで完了通知を出さない。状態変換では対象実行の世代を確認し、古い完了イベントで新しい実行を完了扱いにしない。

### 3.5 キャンセルと終了

停止は会話のキャンセルAPIを使い、接続を保持して次の入力を受け付ける。閉じる操作では承認・認証・接続中のTaskも解放し、回答待ちを残さない。再試行は古い接続の終了後に新しい接続を作り、遅れて届いた結果を無視する。

Paneからの一時的な除去は移動でも起きるため、それだけで接続を終了しない。ラッパーの最終解放または明示的な閉じる処理で終了する。Arenaを閉じる操作とアプリ終了にも同じ終了経路を使う。

タブ・Arenaを閉じるときは会話とタブ専用の接続ストアを解放し、既存のConversationView終了処理と`release_session`による`session/close`を使う。`util::process::Child::kill`はUnixではspawn時に`setsid`したプロセス群全体を`killpg(SIGKILL)`で終了するが、Child自体にはUnixのDropによる終了処理がない。初期化完了前のfuture解放も対象にするため、起動オプション経路には次の所有ガードを追加する。新しい終了レジストリや段階的な終了タイマーは追加しない。

1. transport内でspawn直後、stdioのpipe取得や最初のawaitより前に、Child、`on_app_quit`のSubscription、共有する`exited`フラグを1つのガードにまとめる。
2. 接続のfutureがガードを所有し、初期化成功時に`AcpConnection`へmoveする。キャンセル・初期化失敗・タブ解放でDropされたとき、`exited`がfalseならChildのkill処理を呼ぶ。`AcpConnection::drop`の当該経路はガードに委譲して、重複する終了処理を持たない。
3. 既存のstatus futureが解決した時点で、DropとQuitのkill抑止に使う`exited`を立てる。初期化中のselectと、初期化後の`_wait_task`のどちらで解決しても同じフラグを更新し、stderr等のdrainを待つ前に反映する。取得エラーの場合は終了状態を確定できないため、エラーを記録して以後のPIDによるkillを抑止する。既に回収されたPIDが再利用され、別のプロセス群を終了する危険を避ける方針とする。この方針では、生存中の子に対してstatus取得が失敗した場合にプロセスが残る可能性を受け入れる。
4. Quitコールバックはpidと`exited`の共有ハンドルだけを持ち、Childへの参照や接続の強参照を捕捉しない。Unixでは`exited`がfalseの対象プロセス群へSIGKILLを送り、終了status受信後や取得エラー後のPIDには送らない。ガード解放時にSubscriptionも解放する。

GPUIのshutdownはquit observerを実行した後にwindowsを解放するため、通常の所有関係ならQuitでもDropが連鎖する。現在の`quit()`が送るSIGTERMはPTYだけを対象にしている。上記の`on_app_quit`は、接続の強参照が残っていても終了できる保険として使う。猶予は200msなので、Quit時に非同期の`session/close`を待つ設計にはしない。ガードは起動オプション経路だけで使い、Zedの既定起動経路は維持する。

アダプターまたはApp Serverが別セッションへdetachした場合はプロセス群終了の対象外になるため、実機で両方が終了することを確認する。一般的なプロセス名でkillせず、そのタブが起動したプロセス群だけを対象にする。

## 4 実装の順序

| 段階 | 作業 | 次へ進む条件 |
| --- | --- | --- |
| 0 | ソース変更前にルートREADME.mdの先頭へ必須のレビュー確認2行を追加する。基準差分を確認し、GPUIテストを書く際はgpui-testスキルを読む | 作業中の変更を保持し、READMEの確認行が存在する |
| 1 | LanguageModelRegistryを初期化し、FakeAcpAgentServerでConversationViewをItemに包む。省略したPanel依存の操作も確認する | 入力・逐次応答・承認・モデル／モード・フォーカス・エラー表示が成立する |
| 2 | Codex用起動処理、所有ガード、cwd・クライアント識別のAPI、設定と起動失敗UIを実装する | fake spawnが受け取るprocess cwd・session cwd・注入した明示環境が2つのArenaで一致する。Noneは警告と継承環境になる。初期化中のキャンセルでもプロセス群が終了し、Zedの既定起動を保つ |
| 3 | NewCodexAcpアクション、＋メニュー、通常タブ操作、2点のホスト連携とAgentDiffToolbarを実装する | 同一Arena内の移動でセッションを維持し、差分Paneのボタンから対象スレッドの編集をレビューできる |
| 4 | 状態集計、承認・停止・再試行、所有ガードとタブ／Arena／Quitの終了処理の連携を実装する | 全終了経路で子プロセスが残らず、正常完了・停止・失敗を区別する |
| 5 | 現行の実アダプターで認証・会話・承認・編集・停止を確認し、対象テストと静的チェックを実行する。READMEに設定と制約を追記する | 受け入れ項目が通り、変更した共有クレートの回帰チェックが通る |

段階1でUI流用が成立するまで、本体全体の初期化や独自UIの実装を増やさない。段階2ではモデル推論を伴わない初期化・セッション通信から確認し、実モデルを使う操作は段階5で必要なケースに絞る。段階1～4の検証はfake接続を使い、ネットワークや実アカウントに依存させない。

主な変更先は`agentium/{Cargo.toml,src/agentium.rs,src/arena.rs,src/main.rs,src/codex_acp_view.rs}`、`agent_servers/{src/agent_servers.rs,src/acp.rs,src/acp/transport.rs}`、`agent_ui/{src/conversation_view.rs,src/conversation_view/thread_view.rs,src/ui/mention_crease.rs}`。ホスト連携の型を公開するために必要な再exportだけ`agent_ui/src/agent_ui.rs`へ加える。`AgentDiffToolbar`は公開済みで、`agent_diff.rs`の変更は不要。READMEのレビュー確認行はエージェントが削除しない。

## 5 テストと受け入れ確認

GPUIテストはラッパーとArenaへの接続で起きる動作差に絞り、Zedの会話描画テストを複製しない。Agentiumのdev-dependenciesに`agent_servers`、`agent_ui`、`acp_thread`の`test-support`を加える。`FakeAcpAgentServer`のmode、`fail_next_prompt`、`simulate_server_exit`を使い、必要な承認・セッション記録を既存のfakeで補う。modes非対応のStubAgentConnectionはモデル・モードの検証に使わない。タイムアウトはGPUIのexecutor timerを使う。

`project/test-support`はCLI環境を空のSomeに固定するため、GPUIテストで実ディレクトリ環境の取得は検証しない。起動処理に環境取得の結果を注入し、fake spawnへ届くprocess cwd・session cwd・明示envとNone時の継承動作をassertする。実際の環境取得は段階5で確認する。プロセス群の終了は本物の小さなfake stdio親子プロセスで検証し、実Codexだけに依存させない。

Quitフックの自動検証では`cx.update(|cx| cx.shutdown())`でGPUIのquit observerを実行し、接続の強参照が残っていてもガードが対象プロセス群を終了することを確認する。

| ケース | 自動検証する条件 |
| --- | --- |
| メニューとフォーカス | 1回の選択で1タブ・1ルートセッションを作る。入力欄へフォーカスする |
| 複数タブ | 同じArenaでも別のセッションになり、応答・停止・再試行が混ざらない |
| 設定保存 | PATH起動後にモデル・モードを保存してもcustom設定を維持し、次のタブを起動できる |
| 作業先 | fake spawnで2つのArenaのprocess cwd・session cwd・注入envを記録し、先頭worktreeへの誤起動がない |
| 移動と非表示 | Pane移動・タブ切替・Arena切替で接続を維持する。非表示側の更新がフォーカスを奪わない |
| 承認 | 許可と拒否を返せる。承認待ちが表示され、タブを閉じれば待ちが終わる |
| 実行状態 | 初期Idle、正常終了、停止、失敗、再試行後の古いイベントを区別する |
| ファイルと差分 | 通常リンク、Markdown、mentionの追加先が表示中のPaneになる。タブA・BのAgentDiffPaneでツールバーのKeepAllがそれぞれのaction_logだけに作用する。RejectAllボタンも操作できる |
| 異常終了 | cwd消失、初期化失敗、接続断をタブ内に表示する。環境取得Noneは警告を表示し、継承環境で起動する。未導入案内は注入したExited/status 127で分岐を確認する |
| 終了処理 | spawn直後・initialize応答待ち・認証中・承認待ち・実行中のcloseで解放する。初期化失敗と接続futureのキャンセルでもプロセス群が終了する。終了status受信後と、注入したstatus取得エラー後のDrop/Quitはkillを呼ばず、close対象以外のタブは動作する |
| Quit | 初期化中と接続済みのガードがQuit時に対象プロセス群を明示終了する。接続の強参照が残るケースも検証する |
| Zedの回帰 | 起動オプション・ホスト連携を指定しない呼び出しが従来の値と処理を使う |

実機ではFinder起動時のPATH、異なる2つの作業ディレクトリでの実環境取得、既存Codexログインを利用する起動、未認証からのログイン、APIキー方式、実際のコマンド承認と編集結果、停止後の次の入力、Cmd+W、Arena終了、Quitを確認する。未導入コマンドが実際にExited/status 127として届き、導入案内が出ることも確認する。FakeAcpAgentServerのsimulate_server_exitは既定statusを使うため、127の実transport確認には使わない。adapterとCodex App Serverの両方が終了することも確認する。クライアント名`agentium`で`zed`との差がないことを1回確認する。利用できる認証方式は導入版とアカウントに応じて記録する。検証用の作業ディレクトリを使い、既存タブのプロセスには触れない。

検証コマンドは変更対象に応じて次から選ぶ。

```sh
cargo fmt -p agent_servers -p agent_ui -- --check
cargo check -p agentium -p agent_servers -p agent_ui
cargo check -p zed
cargo test -p agentium codex_acp
cargo test -p agent_servers --lib
cargo test -p agent_ui --lib conversation_view
./script/clippy -p agentium -p agent_servers -p agent_ui
```

Agentiumの既存ファイルには未整形の差分が多いため、全クレートのfmtを完了条件にせず、整形のためだけに既存ファイル全体を書き換えない。新しいRustファイルと変更部分の書式は個別に確認する。`cargo check -p zed`で、共有APIを使用するZed本体もコンパイルする。共有APIの変更が他の呼び出しへ影響する場合は、その既存テストも実行する。完了報告には実アダプターとCodexのバージョン、実施したケース、未実施のケースを記録する。Release Notesの候補は`- Added Codex (ACP) tabs in Agentium.`とする。

## 6 後続開発

初期版が成立した後、NodeRuntimeとACP Registryによる導入・更新、セッションIDを使った履歴復元、`tab new`と`tab send-message`のACP対応、他エージェントを別の計画で扱う。初期版ではこれらのための汎用設定画面やCLIプロトコルを先に追加しない。

## 7 レビュー結果

2026-10-11、Agentiumスキル経由でClaudeに既存APIとの整合、初期化、Arenaの作業先、ホスト連携、Taskとプロセスの寿命、検証条件をレビューしてもらった。Rev 1の必須修正7件、Rev 2の追加2件を反映し、Rev 3で「承認: 必須修正0件」「必須修正1～9はすべて解決済み」と確認された。

承認時の任意指摘に従い、status取得エラー後のkillを抑止する方針とそのトレードオフ、および`App::shutdown`を使うQuitテストを追記した。計画レビューの確定時点では実装に未着手。[その後の実装・検証記録](codex-acp-tab-validation.md)を別途記載した。

## 8 参照先

- [現行Codex ACPアダプター](https://github.com/agentclientprotocol/codex-acp)
- [旧アダプターの移転案内](https://github.com/zed-industries/codex-acp#readme)
- [OpenAI Codex App Server](https://developers.openai.com/codex/app-server)
- [Zed External Agents](https://zed.dev/docs/ai/external-agents)
- [AgentiumのArena実装](../src/arena.rs)
- [ACP接続](../../agent_servers/src/acp.rs)
- [会話UIとItemラッパーのテスト](../../agent_ui/src/conversation_view.rs)
- [ディレクトリ環境](../../project/src/environment.rs)
