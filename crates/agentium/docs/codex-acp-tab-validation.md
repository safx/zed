# Codex ACPタブの実装・検証記録

2026-10-12。実験用ブランチ `experiment/codex-acp-tabs`。計画は[Claude承認済みのRev 3](codex-acp-tab-plan.md)。コードレビュー第2回で必須修正0件、Claude承認済み。

## 実装

Paneの＋メニューに「Codex (ACP)」を追加した。通常のItemとしてConversationViewを包み、タブごとに接続・セッション・作業ディレクトリを持つ。実行・承認・完了・エラーの表示をタブとArenaのバッジに接続した。

AgentiumのCustom設定を読み、未設定ならシェルがbare nameの`codex-acp`を解決する。ログインシェルの完了を待ち、端末と同じ環境取得経路にArenaのcwdを渡す。取得できない場合は継承環境と警告を使う。設定保存は既存のcommand・args・envを維持する。

Zedの既定接続経路を維持し、起動オプションでcwd、クライアント名、プロセス終了ガードを指定できるようにした。ガードはspawn直後から所有し、接続キャンセル・Drop・Quitでプロセス群を終了する。status取得エラー後も再シグナルを抑止し、再利用されたPIDを対象にしない。

Agentium向けホスト処理は通知の表示判定・クリック先・ログイン用端末の追加先を担当する。Markdownとmentionは表示中のcenter paneへ開く。差分タブには既存のAgentDiffToolbarを追加し、Keep All / Reject Allを各threadのaction_logへ接続する。エディタ内のsingle_file_reviewは対象外。

## 計画から追加した修正

- AgentConnectionStoreの接続監視タスクは従来detachされていた。タブ専用の`new_isolated`では保持してキャンセルし、接続中のcloseでfutureを解放する。
- 差分タブがthreadを保持してもadapterを止めるため、専用storeのrelease時に接続プロセスを明示終了する。
- `Pane::take_active_item`で最後のItemを移すとtoolbarが旧Itemを保持していた。空になったPaneでは、`close_pane_if_empty`にかかわらずtoolbarを更新して解放する。既存の呼び出しを1行移動し、既存Paneテスト83件を実行した。
- TerminalPanelが存在しないAgentiumでも、認証が要求する端末を作成元Arenaに開けるようにした。

## 自動検証

| 検証 | 結果 |
| --- | --- |
| `cargo test -p agentium --lib --offline` | 118件成功 |
| `cargo test -p agent_servers --lib --offline` | 58件成功 |
| `cargo test -p agent_ui --lib --offline` | 497件成功 |
| `cargo test -p workspace --lib pane::tests --offline` | 83件成功 |
| `cargo check -p agentium -p agent_servers -p agent_ui -p zed --offline` | 成功 |
| `./script/clippy -p agentium -p agent_servers -p agent_ui --offline` | release・all-targets・all-features・deny warningsで成功 |
| `cargo fmt -p agent_servers -p agent_ui -- --check` | 成功 |
| `rustfmt --edition 2024 --check crates/agentium/src/codex_acp_view.rs` | 成功 |
| `git diff --check` | 成功 |

計756件成功。新規ケースは接続中のキャンセル、タブ・cwdの独立性、Pane移動とセッション解放、thread参照が残るclose、古い完了イベント、設定保存、差分タブ別のKeep All、プロセス群のDrop/Quit、明示cwd/env、未導入コマンドの終了コード127を扱う。レビュー後には起動設定の純粋関数テスト5件と、本番connectの無効ディレクトリ拒否テスト1件を追加した。

## コードレビュー第1回

Claudeは初回実装コミット`37d95929a3`に対して必須修正C1・C2を指摘した。所有ガード、独立storeの寿命、ログインシェル待ち、cwd/envの取得経路、既定Zed経路は問題なしと確認した。

- C1: `build_launch`を切り出し、connectが戻り値のcommand、options、mode、config、warningをそのまま使うようにした。2つのcwdとsession cwdの一致、explicit > proxy > directoryの環境優先順、None時の警告と空env、Registry拒否、Customのcommand/args/env/default保持をテストした。未指定・不存在・通常ファイルの作業先が本番connectで拒否されることも検証した。
- C2: Item削除後の無条件toolbar更新を撤回し、空になったPaneだけで既存の更新を行う。バックグラウンドのタブ削除で検索やBreadcrumbsを再初期化しない。
- I1: 終了コード127のCodex導入案内を、ホストの存在に加えてagent IDがCODEX_IDのときだけ表示する。
- I2: 未使用の`AcpLaunchOptions.client_version`を削除した。client_info.versionには従来どおりアプリのバージョンを使う。

修正後に上記全テスト、Zedを含むcheck、Clippy、fmtを再実行して成功した。別Paneにフォーカスがある場合の承認通知の仕様もREADMEに記載した。手動UI検証の未実施項目は後述のとおり。

## コードレビュー第2回

Claudeは修正コミット`9e1e05db6b`を承認した。必須修正C1・C2とインターフェース指摘I1・I2は解決済み、残る境界契約上の問題もなしと確認した。Claude自身もCodex関連テスト12件とPaneテスト83件を実行し、すべて成功した。Agentiumスキル経由の2回のコードレビューで収束した。

任意指摘として、Registry拒否の本文に設定ファイルの実パスを追加する改善案が残る。現在はCustom設定が必要であることをタブに表示し、設定ファイルのパスはREADMEに記載している。未導入のCodexに対する終了コード127の案内には実パスを表示する。

workspace全体のfmtは基準コミットでも失敗する。`pane.rs`と`persistence.rs`をHEADから一時ファイルへ取り出して同じ失敗を確認したため、既存箇所の整形は含めていない。デバッグテストのリンク時には大きな`__eh_frame`についてApple linkerの警告が出るが、テストは成功する。依存する`block 0.1.6`のfuture-incompatibility通知も残る。

## 実アダプター

一時ディレクトリにインストールした`@agentclientprotocol/codex-acp 2.2.2`と付属の`codex-cli 0.160.1`でstdioプローブを実行した。アプリ本体や既存タブを再起動せず、既存設定も変更していない。

- 未認証ではinitializeが成功し、session/newがエラーを返すことを確認した。
- 既存のファイル認証情報を検証用CODEX_HOMEへコピーし、認証済みセッション作成を確認した。一時認証ファイルは検証終了時に削除し、内容を出力していない。
- `read-only`へのモード変更後、ツールやファイル変更を求めない短いプロンプトを送った。応答のtext chunkと`end_turn`を確認した。
- `read-only`、`workspace-write`、`agent`、`agent-full-access`の4モード、およびmode、collaboration_mode、model、reasoning_effort、fast-modeの設定項目を確認した。
- 認証あり・なしの両方で、クライアント名`agentium`と`zed`のinitialize/session結果が一致した。
- adapterとCodex App Serverを含む子プロセスを記録し、対象プロセス群の終了後に生存していないことを確認した。

`NO_BROWSER=1`のプローブでは認証方式は`api-key`のみ表示された。ブラウザ・端末ログインの実操作はこのプローブの対象外。

## 手動UI検証の残り

Finderから起動した新ビルドのPATH、実際の2つのArenaでの環境差、ブラウザ/端末ログイン、コマンド承認・実編集・Reject All、停止後の再入力、通知クリック、Cmd+W・Arena close・QuitのネイティブUI操作は未実施。関連する会話・Pane・差分・終了処理はGPUIテストとプロセステストで検証した。

プローブのスクリプトと実行ログは`/private/tmp/agentium-acp-smoke.py`と`/private/tmp/agentium-acp-smoke.log`。この一時スクリプトは製品コードに含めない。
