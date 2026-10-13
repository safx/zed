# Codex ACPタブ 第2フェーズの実装・検証記録

2026-10-13。ブランチは `experiment/codex-acp-tabs`、実装前のHEADは `c8d6decbb8`。計画は[Claude承認済みRev 2](codex-acp-phase2-plan.md)。コードレビュー依頼前の記録。

## 実装

- `codex_acp_tabs.json` にworktreeのcanonical path、固定タブ名、タブUUID、セッションIDを保存する。対象Arenaを開いたときにACPタブを同期追加し、従来の非同期初期端末作成も維持する。同じpathのArenaは再作成しない。パス指定の新規プロセス起動も共通のArena作成経路を通す。
- 保存は単一のタスク列で一時ファイルへ書き、renameで置き換える。破損・未対応versionは元ファイルを保持して保存を停止し、読み書きのエラーを画面に表示する。Pane移動では保持し、タブclose・Arena closeでは記録を消す。Quit時は最新のPane構成を反映して以後の更新を止め、最後の保存を待つ。
- 保存IDを `ConversationView` のload経路へ渡す。ロード応答の設定初期化後にforeground通知をdrainし、再生が完了してから呼び出し元へ返す。復元したthreadの世代を基準とし、その後に開始したターンだけをCompletedへ遷移させる。失敗したIDは保持し、既存のエラー・再試行UIと復元案内を表示する。
- 両方の `PaneContentType` にCodexAcpを追加した。CLIは固定名を使い、一覧では `codex-acp` と `idle/running/permission/ready/error` を返す。新規作成時は空名・重複名・末尾commandを拒否し、未指定名を採番する。ACPのpane splitとtab logは非対応。
- 外部メッセージは既存のfrom行を付けてrootの入力欄へ挿入し、submit指定時だけ送信する。実行中、承認待ち、子会話の承認、接続のelicitation、load/auth/error、入力解決中、キュー、下書き・mention・添付を拒否する。接続の生存PIDを祖先照合へ加え、送信者の検証は引き続きkernelのpeer情報を使う。
- `build_launch` が端末と同じホスト識別用envを設定する。Agentiumの既定設定だけでedit/terminalカードを折り畳み、ユーザーの明示trueを優先する。READMEとAgentiumスキルも更新した。

## 自動検証

| コマンド | 最終結果 |
| --- | --- |
| `git diff --check` | 成功 |
| `cargo check -p agentium -p agent_servers -p agent_ui --offline` | 成功 |
| `cargo check -p zed --offline` | 成功 |
| `cargo test -p agentium --lib --offline` | 127件成功 |
| `cargo test -p agent_servers --lib --offline` | 58件成功 |
| `cargo test -p agent_ui --lib conversation_view --offline` | 183件成功 |
| `cargo test -p agentium --bin agentium --offline` | 5件成功。sandbox内では既存peer PIDテストのUnixソケットがEPERMとなり、sandbox外で同じコマンドを再実行して成功 |
| `cargo fmt -p agent_servers -p agent_ui -- --check` | 成功 |
| `./script/clippy -p agentium -p agent_servers -p agent_ui --offline` | release・all-targets・all-features・deny warningsで成功 |
| `cargo build -p agentium --offline` | 成功 |
| `./target/debug/agentium tab new --help` | 成功。codex-acpと固定名の説明を表示 |
| `./target/debug/agentium pane split --help` | 成功 |
| `./target/debug/agentium pane split --type codex-acp` | 期待どおりexit 1。非対応の説明を表示し、IPC送信前に拒否 |

計373件成功。`cargo fmt -p agentium` は実行していない。Agentiumの変更箇所は差分で確認し、新しいファイルとACP viewの整形にはrustfmtを使った。ソース変更前にルートREADME先頭のIMPORTANT 2行を確認し、維持した。

デバッグリンク時のApple linkerの `__eh_frame` 警告と、依存する `block 0.1.6` のfuture-incompatibility通知は前フェーズから継続している。Clippy後段のcargo-shearは未導入のためスクリプトが正常終了する。typos/bufもこのスクリプトでは実行されていない。

### 今回追加・強化した確認

- 保存ファイル不在・破損・未対応version・重複セッション、元ファイル保持、保存失敗の可視化と再試行。
- 複数worktreeの更新、書き込み待ち中の削除、`cx.shutdown()` 後に空snapshotで上書きしないこと。
- ArenaのPaneイベントを通した移動とclose。viewを別途保持しても、閉じたタブの記録を消すこと。
- FakeAcpAgentServerによる保存IDのload、InProgress端末ツールを含む履歴、Idle維持、baseline世代のStatusChanged/StoppedでCompletedにならないこと、復元後のCLI送信と正常完了。
- diffとterminal両方の表示モデルが存在する復元履歴で、展開集合が空であること。新規会話でも初期折り畳み、設定再読込、ユーザーの明示trueによる展開を確認した。
- エージェントが会話タイトルを変更してもタブ名が固定されること。失敗したloadで保存IDを維持し、送信を拒否すること。
- submitなしの下書き保存、既存下書きと各拒否状態で入力を置き換えないこと。共有UIでは入力解決中・キュー・子会話の承認・接続のelicitationを検証した。外部送信の同期呼び出しではフォーカスが変わらないこと。
- CLIのclap/IPC type変換、空・重複・制御文字入りの名前の拒否、未指定名の採番。
- ホスト識別envの通常・取得失敗・競合時の値、プロセス終了後にPIDを返さないこと。
- 既存のload/close/キャンセル/再試行テスト58件。load完了後に追加でexecutorを動かさなくても履歴が揃うよう、既存の再生テストを強化した。設定更新通知がロード応答の初期値で上書きされない既存テストも成功した。

ログは `/private/tmp/agentium-phase2-{check,zed-check,tests,servers-tests,ui-tests,cli-tests,clippy,build}.log` に保存した。

## 実アダプターと未実施項目

mise shim `/Users/mac/.local/share/mise/shims/codex-acp --version` は `@agentclientprotocol/codex-acp 2.2.2`。前フェーズの一時導入先のCodexバイナリは `codex-cli 0.160.1`。後者はsandbox内でPATH alias作成の警告を出したが、バージョン取得はexit 0だった。今回はinitialize・prompt・session/loadの実アダプタープローブを再実行していない。

稼働中アプリの再起動や、既存の会話を閉じる操作は行っていない。以下は未実施。

- ネイティブUIでのQuit→対象Arena再openによる復元。Generating中のSIGKILL後に実アダプターの履歴を読み、未完了ツールが残る場合の入力・エラー表示を確認すること。
- ネイティブUIでのCmd+W・タブ群close・Arena closeと、次回open時に復元されないこと。
- 起動引数からのArena作成、別Arenaを開くまでロードしないこと、同じpath/symlinkからの重複open、保存記録あり・なしの初期端末1つを含む結合検証。これらの経路はソース差分で確認した。
- 新ビルドのGUIに対するCLI new/list/self/send-message、非表示Arenaへの非同期送信後もフォーカスが変わらないこと。
- ACPツールの子プロセスからのホストenv、mise shimを含む祖先照合、検証済みfrom行、daemon化で祖先が切れた場合の未検証表示。
- 実アダプター不在・セッション消失・認証・部分再生後のLoadErrorからの同じIDでの再試行、承認UIの手動操作とカードの手動展開。

## 計画との差分

製品仕様の変更はない。成功時drainは設定応答の初期化後に置き、既存の設定更新順序を維持した。検証は上記自動テストとCLI引数の実操作までで、§5.1のArena作成経路の結合検証と§5.2の実アダプター・ネイティブUI操作は残っている。
