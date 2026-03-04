# Agentium: タブ内 WebView ブラウザ機能 — 調査レポート

**調査日**: 2026-03-04
**質問**: Agentium のタブ内に WebView で URL をレンダリングする簡易ブラウザ機能を追加できるか

---

## 結論

**実現可能**。gpui-component プロジェクトが実証済みのアプローチ (Wry + GPUI) を Agentium に移植できる。

---

## 1. 要件の整理

| 要件 | 優先度 | 実現性 |
|---|---|---|
| アドレスバーでの URL 入力 → ページレンダリング | MUST | 可能 |
| Cmd+R / リロードボタンでの再読み込み | MUST | 可能 |
| 戻る・進む | OPTION | 可能 |
| macOS 対応 | MUST | 可能 (WKWebView ベース) |

---

## 2. 技術スタック選定

### 2.1 Wry (推奨)

[Wry](https://github.com/tauri-apps/wry) は Tauri チームが開発する Rust 製クロスプラットフォーム WebView ライブラリ。

| 項目 | 内容 |
|---|---|
| macOS バックエンド | **WKWebView** (OS 標準、追加バイナリ不要) |
| 最新バージョン | 0.53.5 |
| `raw-window-handle` | **v0.6** 対応 (GPUI と同じ) |
| ライセンス | Apache-2.0 / MIT |
| バイナリサイズ影響 | 小 (OS の WebKit を使用) |

**必要な Wry API — 全て存在を確認済み:**

| メソッド | 用途 | 確認状態 |
|---|---|---|
| `WebViewBuilder::new()` | ビルダー作成 | ✅ |
| `.with_url(url)` | 初期 URL 設定 | ✅ |
| `.with_bounds(Rect)` | 初期サイズ | ✅ |
| `.build_as_child(&window)` | GPUI Window の子として作成 | ✅ |
| `webview.load_url(url)` | URL ナビゲーション | ✅ |
| `webview.reload()` | ページ再読み込み | ✅ |
| `webview.url()` | 現在の URL 取得 | ✅ |
| `webview.set_bounds(Rect)` | 位置・サイズ更新 | ✅ |
| `webview.set_visible(bool)` | 表示/非表示 | ✅ |
| `webview.focus()` / `focus_parent()` | フォーカス管理 | ✅ |
| `webview.evaluate_script(js)` | JS 実行 (history.back() 等) | ✅ |

**戻る・進む**: `go_back()` / `go_forward()` 専用メソッドは存在しないが、`evaluate_script("history.back()")` / `evaluate_script("history.forward()")` で実現可能 (gpui-component でもこの方法を使用)。

### 2.2 vercel-labs/agent-browser (不適)

ヘッドレス Chromium 自動化 CLI ツール。外部 Chromium プロセスを CDP で操作するアーキテクチャで、UI 内埋め込みには不適。

---

## 3. GPUI との統合方法

### 3.1 gpui-component の実証済みパターン

[gpui-component/crates/webview](https://github.com/longbridge/gpui-component/blob/main/crates/webview/src/lib.rs) が Wry + GPUI の統合を実証済み。

**アーキテクチャ概要:**

```
┌─ GPUI Window (HasWindowHandle) ──────────────────┐
│                                                    │
│  ┌─ GPUI Element Tree ─────────────────────────┐  │
│  │  div (track_focus, size_full)                │  │
│  │  ├─ canvas (bounds tracking)                 │  │
│  │  │   → WebView.bounds を更新                 │  │
│  │  └─ WebViewElement (custom Element)          │  │
│  │      → prepaint: set_bounds() で wry を配置  │  │
│  │      → paint: hitbox + mouse event 登録     │  │
│  └──────────────────────────────────────────────┘  │
│                                                    │
│  ┌─ wry::WebView (NSView subview) ──────────────┐  │
│  │  WKWebView (OS ネイティブ)                    │  │
│  │  → GPUI の Element と重なって表示される       │  │
│  └──────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────┘
```

**2層レンダリング方式:**
1. GPUI の `canvas` Element で論理的な bounds を追跡
2. `WebViewElement` の `prepaint()` で wry の `set_bounds()` を呼び、ネイティブ WebView の物理位置を同期
3. `paint()` で hitbox を登録し、WebView 外クリックで親にフォーカスを戻す

### 3.2 GPUI 側の対応状況

| GPUI 機能 | 状態 | ファイル |
|---|---|---|
| `HasWindowHandle` 実装 | ✅ あり | `window.rs:5226-5228` |
| `raw-window-handle` v0.6 | ✅ 一致 | `gpui/Cargo.toml:98` |
| `Element` トレイト | ✅ カスタム実装可 | `element.rs` |
| `canvas` Element | ✅ あり | `elements/canvas.rs` |
| `ContentMask` (クリッピング) | ✅ あり | `window.rs` |
| macOS: `AppKitWindowHandle` | ✅ `native_view` を返す | `mac/window.rs:1628-1636` |

**ポイント**: GPUI の `Window` は `HasWindowHandle` を実装しており、`AppKitWindowHandle` (= NSView ポインタ) を返す。Wry の `build_as_child(&window)` に直接渡せる。

### 3.3 既存の GPUI ネイティブビュー統合

GPUI には以下のネイティブ統合が既に存在:

| 機能 | Element | 方式 |
|---|---|---|
| `Surface` | `elements/surface.rs` | CVPixelBuffer → Metal テクスチャ (非インタラクティブ) |
| `Canvas` | `elements/canvas.rs` | カスタム描画コールバック |

`Surface` はピクセルバッファ描画のみ (インタラクティブなネイティブビューではない)。Wry のアプローチは NSView subview を直接追加する方式で、GPUI の描画パイプラインとは独立。

---

## 4. 実装設計

### 4.1 新規ファイル構成

```
crates/agentium/
├── Cargo.toml              # wry 依存追加
└── src/
    ├── main.rs
    ├── agentium.rs
    └── browser_view.rs     # 新規: ブラウザタブ
```

### 4.2 BrowserView 構造体

```rust
use std::rc::Rc;
use gpui::*;
use workspace::Item;
use wry::Rect;

pub struct BrowserView {
    webview: Rc<wry::WebView>,
    url_input: String,           // アドレスバーのテキスト
    current_url: String,         // 現在表示中の URL
    visible: bool,
    bounds: Bounds<Pixels>,
    focus_handle: FocusHandle,
}

impl BrowserView {
    pub fn new(
        initial_url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let webview = wry::WebViewBuilder::new()
            .with_url(initial_url)
            .with_bounds(Rect::default())
            .build_as_child(window)       // GPUI Window は HasWindowHandle を実装
            .expect("failed to create webview");

        Self {
            webview: Rc::new(webview),
            url_input: initial_url.to_string(),
            current_url: initial_url.to_string(),
            visible: true,
            bounds: Bounds::default(),
            focus_handle: cx.focus_handle(),
        }
    }

    // --- MUST: URL ナビゲーション ---
    fn navigate(&mut self, url: &str) {
        let url = if !url.contains("://") {
            format!("https://{}", url)
        } else {
            url.to_string()
        };
        self.webview.load_url(&url).ok();
        self.current_url = url;
    }

    // --- MUST: リロード ---
    fn reload(&mut self) {
        self.webview.reload().ok();
    }

    // --- OPTION: 戻る・進む ---
    fn go_back(&self) {
        self.webview.evaluate_script("history.back()").ok();
    }

    fn go_forward(&self) {
        self.webview.evaluate_script("history.forward()").ok();
    }

    fn hide(&mut self) {
        self.webview.set_visible(false).ok();
        self.webview.focus_parent().ok();
        self.visible = false;
    }

    fn show(&mut self) {
        self.webview.set_visible(true).ok();
        self.visible = true;
    }
}
```

### 4.3 GPUI Element 統合 (gpui-component パターン)

```rust
impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().clone();

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            // --- アドレスバー ---
            .child(self.render_toolbar(window, cx))
            // --- WebView 領域 ---
            .child(
                div()
                    .flex_1()
                    .child({
                        let view = view.clone();
                        canvas(
                            move |bounds, _, cx| {
                                view.update(cx, |this, _| this.bounds = bounds)
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full()
                    })
                    .child(WebViewElement::new(self.webview.clone(), view))
            )
    }

    fn render_toolbar(
        &self, _window: &mut Window, cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        h_flex()
            .h(px(36.0))
            .px_2()
            .gap_1()
            .bg(colors.title_bar_background)
            .border_b_1()
            .border_color(colors.border)
            // 戻る
            .child(
                IconButton::new("back", IconName::ArrowLeft)
                    .on_click(cx.listener(|this, _, _, _| this.go_back()))
            )
            // 進む
            .child(
                IconButton::new("forward", IconName::ArrowRight)
                    .on_click(cx.listener(|this, _, _, _| this.go_forward()))
            )
            // リロード
            .child(
                IconButton::new("reload", IconName::RotateCw)
                    .on_click(cx.listener(|this, _, _, _| this.reload()))
            )
            // アドレスバー
            .child(
                div().flex_1().child(/* TextInput for URL */)
            )
    }
}
```

### 4.4 Item トレイト実装 (タブとして表示)

```rust
impl Item for BrowserView {
    type Event = DismissEvent; // or custom event

    fn tab_content(&self, params: TabContentParams, ..) -> AnyElement {
        Label::new(self.tab_title())
            .color(if params.selected { Color::Default } else { Color::Muted })
            .into_any_element()
    }

    fn tab_icon(&self, ..) -> Option<Icon> {
        Some(Icon::new(IconName::Globe).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(self.current_url.clone().into())
    }
}
```

### 4.5 タブの表示/非表示の管理

Agentium はタブを切り替えるため、非アクティブなタブの WebView を非表示にする必要がある。
ネイティブ WebView は GPUI のレイアウトとは独立して NSView ツリーに存在するため、タブ切替時に明示的な `show()`/`hide()` が必要。

```rust
// AgentiumWorkspace の pane::Event::ActivateItem ハンドラーで
// 新しいアクティブアイテムが BrowserView かどうかを確認し、
// 前のアイテムが BrowserView なら hide()、新しいアイテムなら show()
```

---

## 5. 重要な考慮事項

### 5.1 座標系の同期

Wry の `set_bounds()` は **ウィンドウ座標系** (論理ピクセル) を使用。GPUI の `Bounds<Pixels>` は Element のレイアウト位置。gpui-component のパターンでは `prepaint()` で GPUI bounds を直接 Wry の `Rect` に変換している:

```rust
// prepaint() 内
self.view.set_bounds(Rect {
    size: dpi::Size::Logical(LogicalSize {
        width: bounds.size.width.into(),
        height: bounds.size.height.into(),
    }),
    position: dpi::Position::Logical(dpi::LogicalPosition::new(
        bounds.origin.x.into(),
        bounds.origin.y.into(),
    )),
}).unwrap();
```

### 5.2 フォーカス管理

WebView がフォーカスを取ると GPUI のキーイベントが奪われる。gpui-component では WebView 外のクリックで `focus_parent()` を呼び、GPUI にフォーカスを戻している。Cmd+R のようなショートカットは WebView がフォーカスを持っている場合、WebView 内で処理される可能性がある。

**対策**: `with_hotkeys_enabled(false)` で WebView のデフォルトショートカットを無効化し、GPUI 側でハンドリングするか、IPC メッセージで通知する。

### 5.3 Z-order (描画順)

ネイティブ WebView は GPUI の Metal レンダリングパイプラインの上に描画される。GPUI のモーダルやポップアップメニューが WebView の下に隠れる可能性がある。

**対策**:
- モーダル表示時に WebView を `hide()` する
- または `set_bounds` でサイズを 0 にする

### 5.4 スクロール・リサイズ

GPUI ウィンドウのリサイズ時、macOS では Wry の WebView が自動リサイズされる (Wry の macOS 実装の挙動)。ただし、Agentium のペイン分割やサイドバーの幅変更時は `prepaint()` で bounds を再同期する必要がある。

---

## 6. 工数見積り

| 作業項目 | 工数 |
|---|---|
| Wry 依存追加 + ビルド確認 | 0.5 日 |
| BrowserView 基本構造 (WebView 作成, 表示) | 1 日 |
| アドレスバー UI + ナビゲーション | 1 日 |
| Cmd+R リロード + ボタン | 0.5 日 |
| 戻る・進む (OPTION) | 0.5 日 |
| タブ切替時の show/hide 管理 | 0.5 日 |
| フォーカス管理・Z-order 対応 | 1 日 |
| **合計** | **4-5 日** |

---

## 7. リスクと制限

| リスク | 影響度 | 対策 |
|---|---|---|
| WebView が GPUI UI 要素の上に描画される | High | モーダル時の hide() |
| フォーカス競合 (GPUI vs WebView) | Medium | `focus_parent()` + hitbox |
| Wry の `raw-window-handle` バージョン不一致 | Low | 現状どちらも v0.6 で一致 |
| WebView のメモリ消費 | Medium | タブごとに 1 WebView、上限設定 |
| macOS 以外のプラットフォーム | Low | 要件により macOS のみで可 |

---

## 8. 主要参考ファイルパス

| 対象 | ファイルパス |
|---|---|
| GPUI Window HasWindowHandle | `crates/gpui/src/window.rs:5226-5228` |
| macOS AppKitWindowHandle | `crates/gpui/src/platform/mac/window.rs:1628-1636` |
| GPUI Element トレイト | `crates/gpui/src/element.rs` |
| GPUI canvas Element | `crates/gpui/src/elements/canvas.rs` |
| GPUI Surface Element | `crates/gpui/src/elements/surface.rs` |
| Agentium タブ追加パターン | `crates/agentium/src/agentium.rs:278-293` |
| raw-window-handle バージョン | `crates/gpui/Cargo.toml:98` (v0.6) |

## 9. 外部参考

| 対象 | URL |
|---|---|
| Wry (WebView ライブラリ) | https://github.com/tauri-apps/wry |
| gpui-component WebView 統合 | https://github.com/longbridge/gpui-component/blob/main/crates/webview/src/lib.rs |
| Wry API ドキュメント | https://docs.rs/wry/latest/wry/ |
| Wry WebView メソッド一覧 | https://docs.rs/wry/latest/wry/struct.WebView.html |
| Wry WebViewBuilder | https://docs.rs/wry/latest/wry/struct.WebViewBuilder.html |
