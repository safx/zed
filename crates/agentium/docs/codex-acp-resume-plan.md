# Codex ACP会話再開UIの実装計画

2026-10-14、Rev 2。ClaudeがR1〜R3の解消と新規must-fixなしを確認し、プランOKを得た。対象はAgentiumのローカルArenaにあるCodex（ACP）タブ。検証結果を添えてコードレビューを依頼する。

## 1 目的と完成条件

ユーザーが閉じたCodexの会話を、現在のworktreeの履歴から選び、履歴付きのACPタブで再開できるようにする。タブバーの「＋」メニューに、既存の「Codex (ACP)」の直後に「Resume Codex Conversation…」を追加する。Cmd+Tで開く同じメニューからも利用できる。

メニューを選ぶと、現在のArenaのworktreeに属する会話を検索できるダイアログを開く。各行にタイトルと更新日時を表示し、更新日時の新しい順に並べる。クリックまたはEnterで新しいタブに開く。同じセッションが起動元Arenaで開いていれば、そのPaneとタブを選択する。他Arenaの生存タブまたは既存の保存記録に同じIDがある場合は、二重登録を避けるため説明を表示して拒否する。会話中のタブを置き換えず、下書きや実行状態を保つ。

接続中・一覧取得中・該当会話なし・検索結果なし・取得失敗を区別して表示する。失敗時はダイアログ内で再試行できる。選択後の履歴ロード失敗は既存のCodexAcpViewの復元エラーとRetryを使う。再開でプロンプトを自動送信したり、中断したコマンドを自動実行したりしない。

## 2 調査結果

`CodexAcpView::with_server`は保存情報からセッションIDを取り出し、`ConversationView::new`へ渡している。ConversationViewはload能力を確認して`load_session`を呼び、非対応時だけ履歴なしの`resume_session`へ進む。今回の再開UIは履歴付きのload対応を必須とする。

`AcpConnection`には`session_list`があり、initialize応答でsessionCapabilities.listがあれば`AcpSessionList`を生成する。`AgentSessionListRequest`はcwdとcursorを指定でき、応答にはID、work_dirs、title、updated_at、next_cursorがある。Agentiumは汎用のThreadMetadataStoreやAgentPanelを初期化していないため、Zedの全履歴インポートUIをそのまま導入しない。

導入済みの`@agentclientprotocol/codex-acp 2.2.2`の配布コードにもlist/load処理を確認した。一覧はCLIなどで作成した会話も含み得る。一部の一覧経路はページを取得した後にcwdで絞るため、sessionsが空でもnext_cursorがあれば取得を続ける必要がある。実アダプターでの今回の一覧取得と再開はまだ実施していない。

既存の保存形式は全worktreeを通じたセッションIDの重複を禁止する。タブを閉じるとAgentiumの自動復元記録は消えるが、アダプターの会話履歴を削除する操作ではない。再開したタブは既存の永続化経路に再登録する。

`agentium task info --update`は、このリポジトリがタスクに紐づいていないと報告した。レビュー先は同じArenaの`Claude`、返信先は`Codex ACP`と確認した。

## 3 実装設計

### 3.1 入口とダイアログ

`ResumeCodexConversation`アクションを追加し、Arenaで受ける。作業ディレクトリを持つArenaだけで実行する。workspaceの既存modal layerにダイアログを表示し、Pickerによるキーボード選択と検索を使う。新しい論理コンポーネントとして`codex_acp_resume.rs`に一覧取得、状態表示、PickerDelegateをまとめる。既存のArenaやCodexAcpViewを大きく分割しない。

ダイアログは起動元ArenaのWeakEntityとworktreeを固定して保持する。Arena切替で取得対象を変えない。確定時には起動元Arenaが存在し、同じworktreeを持つことを確認する。存在しない場合はエラーを表示し、別Arenaにタブを作らない。

### 3.2 一覧用接続と寿命

ダイアログ専用のisolated AgentConnectionStoreを使い、既存のCodexAcpServerから接続する。サーバー生成をcrate内で共有できる最小APIをCodexAcpView側に追加する。新しい会話を作るためのsession/newは呼ばず、initialize後に能力を確認してsession/listだけを呼ぶ。既存のタブの接続を借りないので、生成中の会話やcloseに影響しない。

ダイアログを閉じたら取得Taskと接続storeを解放し、isolated storeの終了処理で一覧用プロセスも終了させる。非同期処理はダイアログを弱参照し、閉じた後の通知や再試行前の結果が反映されないようTaskの所有と世代を管理する。再試行は一覧用接続を作り直す。タブを選択したら別の既存タブ用接続でloadし、一覧用接続は閉じる。1タブ1接続の所有関係を維持する。

接続環境の警告をダイアログにも表示する。listまたはload非対応は説明を表示する。認証が必要なら、新規Codex（ACP）タブでログインしてから再試行する案内を出す。初期版では新しいログインUIを作らず、認証情報や設定を書き換えない。

### 3.3 一覧取得と検索

listのcwdには、session/newとloadに渡しているArena.working_directoryの値をそのまま指定する。背景処理でそのパスをcanonicalizeし、結果が元の表記と異なる場合はcanonicalパスでも取得する。それぞれ独立したcursor列を最後までたどり、セッションIDで結果をマージする。canonicalizeが失敗した場合は元のパスを使う。追加したcanonicalizeはforegroundでは行わない。

next_cursorがなくなるまで順にページを取得する。空のページでも次のcursorへ進む。既出cursorが再登場した場合は、無限ループを防ぐため取得エラーとして表示する。会話のcwdは`work_dirs.ordered_paths().next()`で取得する。PathListは内部のpathsをソートするため、`paths().first()`ではadditional_directoriesをcwdと誤認する可能性がある。cwdと対象worktreeの両方を背景処理でcanonicalizeし、失敗時はそれぞれ元のパスに戻して比較する。一致する会話だけを候補にし、cwdなし・異なるcwdは除外する。保存キーの表記を今回変更しない。

全ページの取得完了後に候補を公開し、updated_atの降順に並べる。日時なしは最後、同順位はタイトルとIDで安定した順にする。タイトルなしは短縮したセッションIDを表示する。検索はローカルのタイトルとIDを対象にし、ネットワークを再取得しない。日時の表示は既存のchronoを使う。

### 3.4 タブ作成、重複防止、保存

確定直前に、生存する全Arenaの`CodexAcpView::saved_tab`とpersistenceの全worktreeの記録を、セッションIDの完全一致で走査する。パスの表記やworktreeの一致を重複検出の条件にしない。起動元Arenaに同じIDの生存タブがあり、他Arenaや別保存キーに同じIDの競合がなければ、そのPaneとタブを選択する。それ以外の生存タブまたは保存記録に同じIDがあれば拒否する。起動元Arenaの生存タブと、そのタブ自身の保存記録の組み合わせは通常の状態として扱う。

拒否時には保存先または開いているArenaを説明する。所属説明にパスを比較する場合だけ、背景処理で両辺をcanonicalizeし、失敗時は元のパスを使う。この非同期の説明準備と重複検出を分離し、確定時の走査とタブ追加の間にはawaitを挟まない。保存スキーマの重複禁止を緩めず、既存記録を移動・削除しない。

新しく開く場合はUUIDを生成し、固定CLI名を既存の`codex_tab_title`で採番する。`add_codex_acp`はsavedがSomeの場合に採番しないため、同じ同期updateの中で既存タイトルを確認して採番し、SavedCodexTab.titleに入れてから渡す。会話タイトルと固定CLI名を混同しない。選択したセッションIDをSavedCodexTabへ渡し、既存の`add_codex_acp`で起動元ArenaのアクティブPaneへ追加する。固定名が保存済みである自動復元の動作を維持する。選択してから作成するまでの処理はforegroundで同期実行し、連打でも同じIDを二重作成しない。

ターミナルのCodex CLIなど、Agentium外のプロセスで同じ会話が開かれているかは検出できない。履歴の同時書き込みを避けるため、同じ会話を複数プロセスで操作しない制約をドキュメントに記載する。

## 4 変更予定

| ファイル | 変更内容 |
| --- | --- |
| `agentium/src/agentium.rs` | アクション登録、全Arenaと全保存記録のID確認、起動元Arenaの既存タブへの移動 |
| `agentium/src/arena.rs` | メニューとアクションハンドラー、再開タブの作成 |
| `agentium/src/codex_acp_view.rs` | 一覧用サーバー生成API、セッションIDの参照API |
| `agentium/src/codex_acp_resume.rs` | ダイアログ、取得処理、検索、状態と再試行 |
| `agentium/src/codex_acp_persistence.rs` | 必要なら保存済みIDの所属検索を追加 |
| `README.md`、Agentium既存ドキュメント | 再開入口と制約、検証結果 |

全履歴表示、別worktreeへの会話移動、履歴削除、fork、CLIのresumeサブコマンド、タブ内の履歴アイコンは今回の対象外。

## 5 検証

GPUIテストにはgpui-testスキルを適用し、遅延が必要な場合はGPUI executorのtimerを使う。ソース変更前にルートREADME先頭のIMPORTANT 2行を確認済みで、維持する。

一覧取得のテストでは、空ページの後に候補がある場合、複数ページ、重複ID、循環cursor、異なるcwd、日時なし、取得失敗を確認する。シンボリックリンクの生パスとcanonicalパスの両方で取得してマージできること、canonicalize失敗時の比較も確認する。additional_directoriesが辞書順でcwdより前に来てもcwdを正しく判定し、cwdが別で追加ディレクトリだけ一致する会話は除外する。

UIのテストでは検索・空状態・非対応能力・認証エラー・再試行・閉じた後の取得結果を確認する。タブ作成では正しいIDとcwdのload、固定名の採番、起動元Arenaの既存タブへの移動、他Arenaの生存タブでの拒否、連続確定時の重複防止、自動復元記録への登録を確認する。

保存の回帰テストでは、別worktreeキーに同じIDの記録がある状態で再開を試み、拒否されて重複が書かれないことを確認する。背景の保存完了後にファイルを再読込し、persistenceがdisabledにならず、元の全worktreeの記録が保持されることを確認する。同じworktreeの別表記キーと、自Arenaの生存タブがない保存記録でも同様に拒否されることを検証する。必要なfakeの拡張は既存のtest-supportに限定する。

`cargo check -p agentium --offline`、関連テスト、`cargo test -p agentium --lib --offline`、`./script/clippy -p agentium --offline`、`git diff --check`を実行する。共有ACP層やagent_uiを変更する場合は、その変更に関係するテストも実行する。既存ファイル全体の整形は避ける。

実アダプターの確認は一覧取得を優先し、既存の会話へのプロンプト送信は行わない。稼働中アプリの再起動や既存タブのcloseはしない。新しいUIの操作に再起動が必要なら未実施として記録し、実装と自動テストの結果を区別する。

## 6 レビュー手順

Claudeへ本計画のmust-fixレビューを依頼する。指摘を固定番号で追跡し、修正後の差分を送って承認まで繰り返す。承認前に実装を始めない。実装後は差分と検証結果を送ってコードレビューを依頼し、must-fixがなくなりClaudeがOKを出すまで修正と再検証を行う。

## 7 プランレビュー対応

Rev 1でClaudeから3件のmust-fixを受けた。Rev 2で次を反映し、Claudeの再レビューで全件の解消を確認した。

| 番号 | 反映内容 |
| --- | --- |
| R1 | session/newと同じcwd表記とcanonical表記で取得し、背景処理でパスを正規化して候補を比較する。重複判定はパス比較に依存させない |
| R2 | `ordered_paths().next()`でcwdを取得し、追加ディレクトリが辞書順で前に来るケースをテストする |
| R3 | 全生存タブと全保存記録をIDで走査し、自Arenaの生存タブ以外への重複は拒否する。拒否後の保存ファイルを再読込し、disabledにならないことをテストする |
