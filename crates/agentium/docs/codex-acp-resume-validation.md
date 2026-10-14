# Codex ACP会話再開UIの実装・検証記録

2026-10-14。計画は[Claude承認済みRev 2](codex-acp-resume-plan.md)。実装と自動検証を完了し、Claudeのコードレビューでmust-fixなし、コードOKを得た。

## 実装

タブバーの「＋」メニューに「Resume Codex Conversation…」を追加した。Cmd+Tで開く同じメニューからも利用できる。作業ディレクトリを持つArenaから履歴選択を実行する。検索できるPickerダイアログに会話タイトルと更新日時を表示し、更新日時の新しい順に並べる。検索はタイトルとセッションIDの部分一致で行う。

ダイアログ専用のisolated AgentConnectionStoreでCodexAcpServerに接続する。session/newは呼ばず、listとloadの能力を確認して履歴を取得する。元のcwdと、背景でcanonicalizeした別表記のcwdについて、それぞれ全ページを取得する。空ページでもcursorがあれば続け、循環cursorは取得エラーにする。会話のcwdをordered_paths().next()から取り出し、背景でパスを正規化して候補を絞る。

確定時に生存する全Arenaのタブと保存済み全worktreeの記録をIDで確認する。自Arenaの生存タブとそのタブ自身の保存記録なら既存タブを選択し、それ以外の一致は拒否する。判定と作成の間にawaitを挟まない。新しいタブのCLI名を同期update内で採番し、保存IDを既存の履歴復元経路に渡す。ロード失敗時は既存のRetryを使う。

一覧取得のエラー、認証の案内、環境取得の警告を表示する。再試行は古い取得Taskをキャンセルして一覧用接続を作り直す。ダイアログを閉じるとTaskとstoreを解放する。稼働中の他タブの接続は借りない。

Pickerは通常、閉じたmodalを再表示用に保持する。履歴ダイアログではreopenableをfalseにして、閉じたときに一覧用接続と未完了の要求を解放する。計画で想定した独立の世代カウンターは設けず、取得から結果反映までを1つの所有Taskにまとめ、再試行前にTaskをキャンセルする。キャンセル後の古い取得が結果を反映しないことをテストした。

Agentium外のCodexプロセスが同じ会話を開いているかは検出できない。利用上の制約をAgentiumのREADMEに記載した。ルートREADME先頭のIMPORTANT 2行は変更していない。

## 自動検証

初回のcargo checkはMetalコンパイラーのキャッシュ書き込みがsandbox内でEPERMとなったため、同じ検証をsandbox外で再実行した。PickerとAsyncWindowContextのAPIの使い方と、テスト属性の名前衝突を修正した。全体テストで見つかったmodal保持による未完了要求の継続と、テストfixtureの不要なPTY起動も修正した。

| コマンド | 最終結果 |
| --- | --- |
| `cargo check -p agentium --offline -j 4` | 成功 |
| `cargo test -p agentium --lib resume_ --offline -j 4` | 関連10件成功 |
| `cargo test -p agentium --lib --offline -j 4` | 全138件成功 |
| `./script/clippy -p agentium --offline -j 4` | release・all-targets・all-features・deny warningsで成功 |
| `rustfmt --edition 2024 --check crates/agentium/src/codex_acp_resume.rs crates/agentium/src/codex_acp_persistence.rs` | 成功 |
| `git diff --check` | 成功 |

既存ファイル全体の整形は行わず、追加したメソッドと新しいコンポーネントを整形した。デバッグリンク時のApple linkerの`__eh_frame`警告と依存する`block 0.1.6`のfuture-incompatibility通知は残る。Clippy後段のcargo-shearは未導入のため正常終了し、typos/bufは実行されていない。

ログは`/private/tmp/agentium-resume-{check-final,focused-tests,tests,clippy}.log`に保存した。

追加したテストは、空ページからの継続、複数ページ、重複ID、循環cursor、日時順、canonicalize失敗時の生パス比較、シンボリックリンク、cwdより辞書順で前に来る追加ディレクトリ、追加ディレクトリだけ一致する候補の除外を扱う。

UI状態のテストは、タイトルとIDでの検索、認証エラー、再試行での新規接続、能力非対応、閉じる操作と再試行による未完了取得のキャンセル、一覧表示でsession/newを呼ばないことを扱う。タブ作成と判定のテストは、選択したIDの維持、固定名の採番、同じArenaの既存タブ検出、他Arenaでの拒否、起動元Arenaの消失・変更を扱う。

保存回帰テストでは別worktree・同worktreeの既存記録・別表記キーへの同じIDの再登録を拒否し、保存完了後の再読込でdisabledにならず、元の全記録が残ることを確認する。

## 実アダプター

導入済みの`@agentclientprotocol/codex-acp 2.2.2`を、認証情報を入れない一時CODEX_HOMEで起動した。initializeの応答でloadSessionがtrue、sessionCapabilitiesにlist・resume・closeなどがあることを確認した。session/listはエラーコード-32000で失敗した。この結果だけでは認証済み環境の一覧取得成功を確認できない。session/new、prompt、既存会話のloadは呼んでいない。プローブのプロセス群は終了した。

## 未実施

認証済み実アダプターでの履歴一覧取得、選択した会話のload、ネイティブUIでのクリック・Enter・Escape・Retry、再開後のQuitとArena再openは未実施。稼働中アプリの再起動や既存タブのcloseは行っていない。

Pickerのconfirmからwindow.rootでAgentiumAppを取得し、resume_codex_conversationでArenaを切り替え、タブ追加後にsync_codex_tabsで保存へ反映する経路全体は自動テストで通していない。resume_targetとresume_codex_tabは個別に検証済みだが、この配線はネイティブUIでの手動確認が必要。

## コードレビュー

Claudeが実装差分と新しいコンポーネント、検証ログを確認し、コードOKと回答した。must-fixはなかった。R1のパス別ページ取得と背景でのcanonicalize、R2のcwd順序、R3の全Arena・全保存記録のID照合と同期的なタブ追加・保存を確認した。modal終了と再試行時の接続・Task解放、confirmのGPUI借用も問題なしと判断した。

非ブロッキングの補足として、confirmからArena切替・保存までの配線を実機未確認項目に明記する指摘を反映した。確定時の競合拒否でもRetryが表示され、押すと一覧を再取得する点については、変更不要とされた。

## 追加したタブ名編集

その後の依頼で、Codex（ACP）タブのダブルクリックによる名前編集を追加した。既存PaneのRenameアクション経路を使い、Enterで確定、Escapeとフォーカス移動で取消にする。空の名前・制御文字・同じArena内の重複名を拒否し、確定した名前を固定CLI名と保存記録に反映する。セッションIDとタブUUIDは維持する。

描画されたタブをダブルクリックしてEnterで確定するテスト、Escapeとフォーカス移動による取消、不正な名前の拒否、保存後の読み戻しを追加した。最終の`cargo test -p agentium --lib --offline -j 4`は全140件成功、`./script/clippy -p agentium --offline -j 4`と`git diff --check`も成功した。ログは`/private/tmp/agentium-rename-{all-tests,clippy-final}.log`に保存した。

この追加変更は上記のClaudeレビューの対象外。ユーザーが再開UIとタブ名編集を含む変更にLGTMを出し、コミットを依頼した。
