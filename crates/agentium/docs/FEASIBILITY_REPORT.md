# AIエージェント並行作業特化ターミナルアプリ — 実現可能性調査レポート

**調査日**: 2026-02-27
**調査対象**: Zed コミット `88df73c8b5` (main)
**調査方法**: Zedコードベースの実地検証 + 外部技術調査

---

## エグゼクティブサマリー

4つの戦略を比較分析し、コードベース上の具体的なファイル・行番号で検証した。元の計画の大部分は正確だが、いくつかの重要な修正点がある。

### 主要な修正点

| 計画の主張 | 実際 |
|---|---|
| Zedのクレート数: 180+ | **232** (crates/ 225 + extensions/ 5 + tooling/ 2) |
| AI/LLMクレート: 30+ | **~46** (大幅な過小評価) |
| GPUIの例: 35+ | **~30** (ファイル数。Cargo.toml登録は17) |
| GPUIはcrates.ioでスタンドアロン利用可能 | **不可**。内部依存クレート(collections, http_client等)が未公開 |
| action_logに`accept_all_edits`あり | **なし**。`reject_all_edits`のみ。"accept"は`KeepAll`アクション |
| BranchDiffはgit_uiにある | **project/src/git_store/branch_diff.rs** にある |
| Toast/ToastLayerが汎用通知UIとして存在 | `AnnouncementToast`のみ（プロモーション用）。汎用トーストなし |
| bellにオーディオあり | **なし**。`Sound`列挙型にBellバリアントなし。純粋にビジュアル(タブダーティフラグ) |

---

## 1. Zedコードベース構造 — 検証済み

### クレート数と分類

| カテゴリ | 計画の主張 | 実際のクレート数 | 主要クレート |
|---|---|---|---|
| **AI/LLM** | 30+ | **~46** | `agent`, `anthropic`, `copilot`, `language_model`, `edit_prediction`×6, LLMプロバイダー12種, `acp_*`, `eval`等 |
| **コラボレーション** | 5+ | **6** | `call`, `channel`, `collab`(AGPL), `collab_ui`, `livekit_api`, `livekit_client` |
| **エディター関連** | 20+ | **~22** | `editor`, `multi_buffer`, `language`, `languages`, `vim`, `diagnostics`, `search`, `lsp`等 |
| **デバッガー** | — | **5** | `dap`, `dap_adapters`, `debug_adapter_extension`, `debugger_tools`, `debugger_ui` |
| **自動更新/テレメトリ** | — | **5** | `auto_update`×3, `telemetry`, `telemetry_events` |
| **リモート** | — | **4** | `remote`, `remote_connection`, `remote_server`, `dev_container` |
| **ターミナル** | — | **2** | `terminal`(GPL-3.0), `terminal_view`(GPL-3.0) |
| **GPUI** | — | **10** | `gpui`(Apache-2.0), `gpui_macos/linux/windows/web/wgpu`, `gpui_macros`, `gpui_platform`, `gpui_util`, `scheduler` |
| **Git** | — | **4** | `git`, `git_ui`, `git_graph`, `git_hosting_providers` |
| **合計** | 180+ | **232** | — |

### ライセンス分布（全220+クレート）

| ライセンス | クレート数 | 対象 |
|---|---|---|
| **GPL-3.0-or-later** | **194** | アプリケーションコードの大部分 |
| **Apache-2.0** | **25** | GPUI群 + 低レベル再利用ライブラリ |
| **AGPL-3.0-or-later** | **1** | `collab`（サーバーバックエンド） |

**重要**: `terminal`と`terminal_view`はGPL-3.0。GPUIはApache-2.0。`alacritty_terminal`本体はApache-2.0だが、ZedのラッパーコードがGPL-3.0。

### 異常検出

`crates/copilot_ui/` がディスク上に存在するが `Cargo.toml` の `members` リストに含まれていない（`workspace.dependencies`にのみ参照）。ビルド対象外。

---

## 2. 戦略A: Zedフォーク — 機能別検証結果

### 2.1 スプリットペイン — **Low** ✅ 検証済み

**再利用率: 90%+** — 計画通り。

`TerminalPanel`は独自の`PaneGroup`を保持（`crates/terminal_view/src/terminal_panel.rs:79`）。以下のアクションが全て実装・接続済み:

| アクション | ファイル:行 |
|---|---|
| `SplitLeft/Right/Up/Down` | terminal_panel.rs:29-34（import）, 202-205（メニュー） |
| `ActivatePaneLeft/Right/Up/Down` | terminal_panel.rs:1505-1527 |
| `ActivateNextPane/PreviousPane` | terminal_panel.rs:1529-1579 |
| `SwapPaneLeft/Right/Up/Down` | terminal_panel.rs:1581-1592 |
| `MovePaneLeft/Right/Up/Down` | terminal_panel.rs:1593-1604 |

`PaneGroup::split()`メソッドは `workspace/src/pane_group.rs:58-91` に実装。

**必要な作業**: `TerminalPanel`をドックからワークスペース中央に昇格させるレイアウト変更のみ。

---

### 2.2 縦型タブサイドバー — **Medium** ✅ 検証済み

**再利用率: 60%** — 計画通り、ただし1点補足あり。

`Sidebar`クレート（`crates/sidebar/src/sidebar.rs:696`）は`MultiWorkspace`と`Picker<WorkspacePickerDelegate>`を保持。`notified_workspaces: HashSet<usize>`（line 158）でエージェントスレッドの完了を追跡し、`ThreadItem::new(...).generation_done(has_notification)`でリスト項目に視覚インジケータを表示。

**補足**: 現在のサイドバーは`MultiWorkspace`（マルチプロジェクトウィンドウ）用であり、単一ワークスペースのジェネリックな縦型サイドバーではない。ターミナルタブに転用するには`WorkspacePickerDelegate`を`TerminalTabDelegate`に置換する必要がある。

---

### 2.3 Notification Rings（ペイン枠グロー） — **Low-Medium** ✅ 検証済み

**再利用率: 80%** — 計画通り。

| コンポーネント | ファイル:行 | 状態 |
|---|---|---|
| `LeaderDecoration` | pane_group.rs:316-320 | `Option<Hsla>`ボーダー色 + `Option<AnyElement>`ステータスボックス |
| `PaneLeaderDecorator`トレイト | pane_group.rs:322-326 | `decorate()`, `active_pane()`, `workspace()` |
| コラボ実装 | pane_group.rs:355-476 | `follower_states`からリーダー色を取得、`fade_out()`適用 |
| `BoxShadow` | gpui/src/style.rs:313 | `color: Hsla`, `offset: Point<Pixels>`, `blur_radius: Pixels`, `spread_radius: Pixels` |

グロー効果は`PaneLeaderDecorator::decorate()`を拡張し、通知状態に応じて`box_shadow`を追加すれば実現可能。

---

### 2.4 通知システム

#### 4a. サイドバーバッジ — **Low** ✅

`Indicator`コンポーネント（`crates/ui/src/components/indicator.rs`）に3バリアント:
- `IndicatorKind::Dot` — 小さな塗りつぶし円
- `IndicatorKind::Bar` — 水平バー
- `IndicatorKind::Icon(AnyIcon)` — アイコン

#### 4b. 通知ポップオーバー — **Medium** ⚠️ 修正あり

**計画の主張**: `ToastLayer`（10秒自動消去）が既存。
**実際**: 汎用`ToastLayer`は**存在しない**。`AnnouncementToast`のみ（プロモーション用の固定レイアウトコンポーネント）。`PopoverMenu`は存在する（`ui/src/components/popover_menu.rs`）が、通知集約UIは完全に新規実装が必要。

#### 4c. macOSデスクトップ通知 — **High** ✅ 検証済み

`Platform`トレイト（`gpui/src/platform.rs:113-229`）に通知APIは一切ない。macOSプラットフォーム実装（`gpui_macos/src/platform.rs`）にも`UNUserNotificationCenter`の利用なし。`NSNotificationCenter`はCocoa内部イベント（キーボードレイアウト変更、サーマル状態変更）にのみ使用。

---

### 2.5 OSC 9/99/777 — **High** ✅ 検証済み

`AlacTermEvent`の全バリアント（`terminal/src/terminal.rs:922-997`で処理）:

```
Title, ResetTitle, ClipboardStore, ClipboardLoad, PtyWrite,
TextAreaSizeRequest, CursorBlinkingChange, Bell, Exit,
MouseCursorDirty, Wakeup, ColorRequest, ChildExit
```

**`DesktopNotification`バリアントは存在しない**。`Bell`のみがタブのダーティフラグを設定（`terminal_view.rs:1000-1003`）。音声なし（`audio/src/audio.rs`の`Sound`列挙型にBellなし）。

alacrittyフォーク（`zed-industries/alacritty` rev `9d9640d4`）のVTEパーサー拡張が必要。

---

### 2.6 CLI/フック通知トリガー — **Medium** ✅ 検証済み

`CliRequest`列挙型（`cli/src/cli.rs:12-25`）は現在`Open`バリアントのみ。`IpcHandshake`で`ipc_channel`を使用。`open_listener.rs`で`zed-cli://`プロトコル経由の接続を処理。

**必要な作業**: `CliRequest::Notify { pane_id, title, body }`バリアントの追加。プロセス境界をまたぐペイン識別子の設計（現在`EntityId`はプロセス内部のみ）。

---

### 2.7 組み込みブラウザ — **Very High** ⚠️ ✅ 検証済み

**GPUIにWebViewは一切存在しない**。`wry`や`webview`への依存はCargo.toml/Cargo.lockのどこにもない。

`gpui_web`はGPUI自体をWASMとしてブラウザ内で動かすバックエンド（`WebPlatform`, `WebWindow`等を`web_sys`経由で実装）であり、GPUI内にブラウザを埋め込むものではない。

**V1ではスコープ外とし、`platform.open_url()`でシステムブラウザに委譲**する推奨を維持。

---

### 2.8 ソケットAPI — **Medium** ✅

既存IPCは`ipc-channel`ベースのTCP/Unixソケット。`ActionRegistry::build_action(name, json)`が存在するか要確認だが、基本的なアーキテクチャは流用可能。

---

### 2.9 キーボードショートカット — **Low** ✅ 検証済み

`KeymapFile`（`settings/src/keymap_file.rs:57-59`）: JSON設定、コンテキスト述語、`KeymapSection`。
`KeymapEditor`（`keymap_editor/src/keymap_editor.rs:105`）: フルGUIエディタ、`EditBinding`, `CreateBinding`, `DeleteBinding`, コンフリクトフィルタリング、キーストローク検索。

---

## 3. エディター・Diff・Git機能 — 検証結果

### 保持推奨クレートの存在確認

| 機能 | クレート | ファイル | 確認状態 |
|---|---|---|---|
| インラインDiff | `buffer_diff` | buffer_diff/src/buffer_diff.rs | ✅ word-level diff対応（`buffer_word_diffs`, `base_word_diffs`） |
| ストリーミングDiff | `streaming_diff` | streaming_diff/src/streaming_diff.rs | ✅ LCS動的計画法、`push_new()`で逐次diffを返す |
| FileDiffView | `git_ui` | git_ui/src/file_diff_view.rs:28 | ✅ |
| MultiDiffView | `git_ui` | git_ui/src/multi_diff_view.rs:25 | ✅ |
| ProjectDiff | `git_ui` | git_ui/src/project_diff.rs:71 | ✅ |
| CommitView | `git_ui` | git_ui/src/commit_view.rs:63 | ✅ |
| FileHistoryView | `git_ui` | git_ui/src/file_history_view.rs:29 | ✅ |
| GitPanel | `git_ui` | git_ui/src/git_panel.rs:615 | ✅ |
| BranchPicker | `git_ui` | git_ui/src/branch_picker.rs | ✅ |
| ConflictAddon | `git_ui` | git_ui/src/conflict_view.rs:17 | ✅ |
| SendReviewToAgent | `editor` | editor/src/actions.rs:854 | ✅（git_uiのproject_diff.rsから使用） |
| BranchDiff | `project` | **project/src/git_store/branch_diff.rs:36** | ⚠️ git_uiではなくprojectに所在 |
| GitGraph | `git_graph` | git_graph/src/git_graph.rs:836 | ✅ DAG可視化、レーンベースレイアウト |
| ActionLog | `action_log` | action_log（詳細下記） | ✅（一部修正あり） |
| AgentDiffPane | `agent_ui` | agent_ui/src/agent_diff.rs:41 | ✅ |
| MultiBuffer | `multi_buffer` | multi_buffer/src/multi_buffer.rs:74 | ✅ SumTree<Excerpt>ベース |
| ProjectSearch | `search` | search/src/project_search.rs:231 | ✅ MultiBuffer + regex |
| MarkdownPreview | `markdown_preview` | 6ソースファイル | ✅ |
| ImageViewer | `image_viewer` | 3ソースファイル | ✅ |
| SVGPreview | `svg_preview` | 2ソースファイル | ✅ |
| MCPクライアント | `context_server` | client.rs, protocol.rs, transport.rs | ✅ JSON-RPC MCP実装 |
| WASM拡張 | `extension_host` | wasmtime + WITバインディング | ✅ |

### ActionLog詳細 — ⚠️ 修正あり

| 要素 | 計画の主張 | 実際 |
|---|---|---|
| `TrackedBufferStatus` | 公開enum | **非公開**enum (Created/Modified/Deleted) |
| `ChangeAuthor` | 公開enum | **非公開**enum (User/Agent) |
| `accept_all_edits()` | 存在する | **存在しない**。`reject_all_edits`のみ。"承認"は`KeepAll`アクション（agent_ui側） |
| `reject_all_edits()` | 存在する | ✅ action_log.rs:811 |

---

## 4. GPUI独立利用 — 戦略B検証

### crates.io公開状態

`gpui` v0.2.2 は `publish = true` でcrates.ioに公開済み（Apache-2.0）。

**ただし、スタンドアロンでは使用不可能**。以下の内部依存クレートがcrates.io未公開:

| 内部依存 | `publish` | crates.io |
|---|---|---|
| `gpui_macros` | false | ❌ |
| `collections` | false | ❌ |
| `http_client` | false | ❌ |
| `refineable` | false | ❌ |
| `sum_tree` | false | ❌ |
| `util_macros` | 未公開 | ❌ |
| `gpui_util` | false (inherited) | ❌ |
| `scheduler` | false (inherited) | ❌ |

**実用的な方法**: Zedリポジトリをクローンし、GPUIをpath依存で参照する新規ワークスペースを作成。`gpui_web/examples/hello_web/`がこのパターンの実例。

### サンプル数

計画は35+と主張。実際は**~30ファイル**（`crates/gpui/examples/`内）、うちCargo.toml登録は17。`hello_world`, `input`, `uniform_list`, `animation`, `drag_drop`は全て存在。

---

## 5. 外部技術調査 — 戦略C・D

### 5.1 alacritty_terminal（スタンドアロン）

| 項目 | 内容 |
|---|---|
| crates.io名 | `alacritty-terminal`（ハイフン） |
| 最新安定版 | **0.25.1**（2026年初頭） |
| ライセンス | **Apache-2.0** |
| 直接利用 | **可能**。Zedフォークなしで基本機能は動作。フォークの2パッチはmacOS固有のエッジケース修正（シグナル終了ステータス、PTY `pre_exec`リセット） |
| Zedの現状 | `zed-industries/alacritty` rev `9d9640d4`。上流PR #8825, #8835 がマージされれば復帰予定 |

### 5.2 WezTerm（戦略D）

| 項目 | 内容 |
|---|---|
| ライセンス | **MIT** ✅ |
| メンテナンス | 個人プロジェクト（Wez Furlong氏）。nightly配布が主。安定リリースは不定期 |
| GPU描画 | OpenGL/Metal/Vulkan（カスタムレンダラー） |
| タブ+分割 | 既存。組み込みマルチプレクサ |
| Lua設定 | 成熟した設定API。ただし外部プロセスからのプログラマティック制御は限定的（`wezterm cli`コマンドのみ、完全なIPC/RPCなし） |
| OSC通知 | OSC 9, OSC 777サポート済み |
| UIシステム | カスタムGPU描画。ウィジェットツールキットではない。リッチUIコンポーネント（サイドバー等）は自前描画必要 |
| 注意点 | ZedはWezTerm依存を2024年3月に**ビルド時間削減のため削除**（コミット`41d8ba12ec`） |

### 5.3 Tauri v2（戦略C）

| 項目 | 内容 |
|---|---|
| 安定版 | v2.0（2024年10月リリース）。プロダクション対応 |
| macOS通知 | `tauri-plugin-notification`で`UNUserNotificationCenter`をラップ。数行で実装可能 |
| xterm.js性能 | キー入力レイテンシ~10-30ms（alacritty_terminalはサブミリ秒）。大量出力時のスループットはJS/IPCでCPU制限 |
| xterm.js制約 | kittyキーボードプロトコル未対応、CJK IME問題、サブピクセルフォントレンダリングなし、高速スクロールでスタッター |
| ブラウザ統合 | **自然に解決**（Tauri自体がWebView） |

---

## 6. フォーク戦略A — 検証済み削除・保持リスト

### 削除可能なクレート（推定88+クレート）

| カテゴリ | クレート数 | 削減効果 |
|---|---|---|
| AI/LLM | ~46 | **最大** |
| コラボレーション | 6 | 大 |
| デバッガー | 5 | 中 |
| REPL | 1 | 小 |
| 自動更新 | 3-5 | 小 |
| テレメトリ | 2 | 小 |
| リモート | 4 | 中 |
| AI固有のLLMプロバイダー | 12 | 大 |
| Eval | 2 | 小 |

### 保持するクレート（~50クレート）

```
# UIフレームワーク（必須）
gpui, gpui_platform, gpui_macos/linux/windows
gpui_macros, gpui_util, gpui_wgpu, scheduler

# ワークスペース・レイアウト
workspace（刈り込み）
sidebar（改造）
panel, pane（workspace内）

# ターミナル
terminal, terminal_view

# エディター + Diff
editor                     # Diff表示の基盤（10万行超だが必要）
multi_buffer               # 仮想ドキュメント合成
buffer_diff                # Diffエンジン
streaming_diff             # ストリーミングdiff
rope, text                 # テキストデータ構造
language（刈り込み）        # 構文ハイライト

# Git統合
git, git_ui                # 全Diff/Gitビュー
git_graph                  # コミットDAG

# エージェント変更追跡
action_log                 # 変更追跡・拒否
# agent_ui（AgentDiffPaneのみ抽出、残りは削除）

# タスク・検索
task, tasks_ui
search

# プレビュー
markdown_preview

# 設定・テーマ・UI
settings, settings_ui
theme, ui, component
picker

# 永続化・基盤
db, sqlez, sqlez_macros
fs
project（刈り込み）
cli（拡張）
paths, util, collections
audio（ベル音 — 現在は未実装だが基盤あり）
keymap_editor
```

**推定効果**: コードベース40-50%削減。ビルド時間15-25分→8-12分。

---

## 7. 検証済み全戦略サマリーテーブル

| # | 機能 | A: Zedフォーク | B: GPUI新規 | C: Tauri | D: WezTerm |
|---|---|---|---|---|---|
| 1 | 縦型タブサイドバー | Medium (60%再利用) | High (自前) | Low (Web UI) | Medium (自前描画) |
| 2 | スプリットペイン | **Low (90%+再利用)** ✅ | High (自前15k行) | Medium (Web flexbox) | **Low (既存)** |
| 3 | Notification Rings | Low-Med (80%再利用) ✅ | Medium (自前) | **Low (CSS glow)** | Medium (OpenGL) |
| 4a | サイドバーバッジ | Low (Indicator既存) ✅ | Medium | **Low (CSS)** | Medium |
| 4b | 通知ポップオーバー | Medium ⚠️汎用Toastなし | Medium | **Low (Web UI)** | Medium |
| 4c | macOS通知 | High (Platform拡張必要) ✅ | High | **Low (Tauri API)** | Medium (notify-rust) |
| 5 | OSC 9/99/777 | High (alacrittyフォーク拡張) ✅ | High (同) | Medium (xterm.js addon) | **Low (既存)** |
| 6 | CLI通知トリガー | Medium (IPC拡張) ✅ | Medium | Medium | Medium |
| 7 | 組み込みブラウザ | **Very High** ✅確認済み | **Very High** | **なし（自然に解決）** | High (wry統合) |
| 8 | ソケットAPI | Medium | Medium | Medium | Medium (wezterm cli基盤) |
| 9 | エージェント非依存 | ✅ PTY | ✅ PTY | ✅ node-pty | ✅ PTY |
| 10 | キーボード | **Low (95%再利用)** ✅ | Medium (自前) | Medium (Web制約) | **Low (Lua既存)** |

---

## 8. 推奨と次のステップ

### 推奨は変わらず — 要件優先度で選択

| 最優先要件 | 推奨戦略 | 理由 |
|---|---|---|
| 組み込みブラウザが必須 | **C (Tauri)** | WebViewが自然に統合。ただしターミナル品質に妥協 |
| ネイティブターミナル品質 | **A (Zedフォーク)** | ペイン分割・キーマップ・永続化が即座に動作。検証で90%+再利用を確認 |
| 長期メンテナンス性 | **B (GPUI新規)** | 技術負債なし。ただしGPUIは真にスタンドアロンではなく、Zedリポジトリへのpath依存が必要 |
| ターミナル品質+低工数バランス | **D (WezTerm)** | ターミナルとしての完成度が高い。ただしリッチUI追加は困難 |

### 戦略A（Zedフォーク）の初期検証ステップ

1. **クレート削除テスト**: AI/collab/remoteクレートを`Cargo.toml`の`members`から削除してビルドが通るか確認
2. **TerminalPanel中央配置**: `TerminalPanel`をドックパネルからワークスペース中央ペイングループに移動
3. **グロー効果**: `PaneLeaderDecorator`に`BoxShadow`ベースのグロー追加
4. **IPC通知**: `CliRequest`に`Notify`バリアント追加

### 戦略B（GPUI新規）の初期検証ステップ

1. Zedリポジトリ内に新規クレート作成（path依存でGPUI参照）
2. `alacritty-terminal = "0.25.1"`をcrates.ioから直接追加
3. カスタム`Element`でターミナルセル描画プロトタイプ作成

---

## 付録: 検証に使用した主要ファイルパス

| 対象 | ファイルパス |
|---|---|
| TerminalPanel PaneGroup | `crates/terminal_view/src/terminal_panel.rs:78-79` |
| PaneGroup split | `crates/workspace/src/pane_group.rs:58-91` |
| LeaderDecoration | `crates/workspace/src/pane_group.rs:316-476` |
| Sidebar MultiWorkspace | `crates/sidebar/src/sidebar.rs:696` |
| Terminal bell | `crates/terminal_view/src/terminal_view.rs:128-129, 1000-1003` |
| AlacTermEvent variants | `crates/terminal/src/terminal.rs:922-997` |
| PtyProcessInfo cwd | `crates/terminal/src/pty_info.rs:86-201` |
| CliRequest IPC | `crates/cli/src/cli.rs:12-25` |
| TaskTemplate | `crates/task/src/task_template.rs:22-75` |
| KeymapFile | `crates/settings/src/keymap_file.rs:57-59` |
| KeymapEditor | `crates/keymap_editor/src/keymap_editor.rs:105` |
| BoxShadow | `crates/gpui/src/style.rs:313` |
| Platform trait | `crates/gpui/src/platform.rs:113-229` |
| Indicator | `crates/ui/src/components/indicator.rs` |
| PopoverMenu | `crates/ui/src/components/popover_menu.rs` |
| buffer_diff | `crates/buffer_diff/src/buffer_diff.rs` |
| streaming_diff | `crates/streaming_diff/src/streaming_diff.rs` |
| ActionLog reject | `crates/action_log/src/action_log.rs:811` |
| AgentDiffPane | `crates/agent_ui/src/agent_diff.rs:41` |
| MultiBuffer | `crates/multi_buffer/src/multi_buffer.rs:74` |
| ProjectSearch | `crates/search/src/project_search.rs:231` |
| alacritty fork ref | `Cargo.toml:482` (git rev 9d9640d4) |
| GPUI version | `crates/gpui/Cargo.toml` (v0.2.2, publish=true) |
