# AI-DLC 質問票ビュー 編集ロック 実装プラン

Rev 4 (2026-08-31)

改訂履歴:

- Rev 4: 3 巡目レビューを反映 (Nit 2 件、Major 以上は無し)。`___` だけの入力は `parse_answer` が未回答扱いに
  するため「非空入力なら下向き遷移は起きない」の断定を弱め §6 に記録、`menu::Cancel` 経路の keymap 根拠を
  §2 に追加。コード設計の変更なし。
- Rev 3: 2 巡目レビュー (Major 2 / Minor 1 / Nit 2) を反映。手動ロック時に `commit_editing` がフォーカスを
  戻さず描画されない Editor にフォーカスが残る (F1)、入力欄が空のまま Lock を押すと `with_other("")` で
  既存の選択が消え、下向き遷移でロックも取り消される (F2) の 2 件を、`toggle_lock` を「空入力なら
  `cancel_editing`、非空なら `commit_editing` + 再フォーカス」に変えて解消。再フォーカス条件を
  `editing.is_some()` から `text_input` が実際にフォーカスを持つかに変更 (F3、`Blurred` は次の draw まで
  遅れるため)。テストの `run_until_parked` 抜けと fixture の空行を修正、手動シナリオ 4 を同一ウィンドウ内に
  限定 (Nit)。テスト 1 本追加。
- Rev 2: 1 巡目レビュー (Major 2 / Minor 3 / Nit 2) を反映。`.tooltip` に必要な `ButtonCommon` を import に追加
  (F1)、遷移検出を対称化して外部追記 (n, n) → (n, n+1) でロックが固着しないようにした (F2)、`reparse` に
  `Window` を渡し `cancel_editing` で明示的にフォーカスを戻す (F3。GPUI は描画されない Editor から
  フォーカスを外さず、アプリでは `Workspace::on_focus_lost` が救済しているだけだった)、手動ロック時は
  入力中テキストを確定してからロックする (F4)、質問 0 件テストの誤った主張を修正 (F5)、コード片の
  説明コメントを削除 (F6)。テストを 2 本追加。
- Rev 1: 初版。

## 1. 目的とスコープ

`QuestionnaireView` で全質問が回答済み (質問 1 件以上、`answered == total`) になった時点で選択肢と
自由記述のクリックを無効化し、誤クリックによる回答の書き換えを防ぐ。ヘッダのロックボタンで
手動解除・再ロックできる。

スコープ内:

- `QuestionnaireView` にロック状態 (`locked: bool`) を追加
- ヘッダ "n/n answered" の右に Lock / LockOff の `IconButton` (クリックでトグル)
- `reparse` での自動ロック / 自動解除 (完了状態が変わった時。ビューを開いた瞬間に 100% の場合を含む)
- ロック中の書き込み抑止 (`write_answer` guard) と見た目 (Checkbox disabled、cursor、自由記述欄)
- 編集中にロックがかかった場合の入力欄の後始末 (外部変更と空入力は破棄、非空の手動ロックは確定)
- GPUI テスト 8 本

スコープ外:

- ロック状態の永続化 (settings / DB)。タブを閉じると消える
- "Open as text" で開いた Editor 側の保護
- キーバインド / action によるトグル (ボタンのみ)
- AI-DLC 側 (ファイル書き込み) の変更
- パーサ・シリアライザ (純関数層) の変更。空入力の Enter / blur が `Single` の選択を消す既存挙動も変えない

## 2. 確認済みの前提

行番号は `crates/agentium/src/questionnaire_view.rs` (HEAD `a6007baac0`)。

| 事実 | 根拠 |
|---|---|
| 進捗は `progress()` が `(answered, total)` を返す。answered は `!parse_answer(block).is_empty()`。`[Answer]:` 行が無い質問は `parse_answer` が空を返すので常に未回答 | `:699-711`, `:401-409` |
| ヘッダは `h_flex().justify_between().items_center()` に `Label("{answered}/{total} answered")` と `Button("open-as-text")` の 2 子。`WithRemSize` の外にあるので cmd-+ の影響を受けない | `:1096-1110`, `:1113` |
| `reparse()` は buffer の `Edited / Reloaded / DirtyChanged / Saved / FileHandleChanged` と生成時に呼ばれ、`self.document` を丸ごと差し替える。初期 `document` は `default()` で質問 0 件。呼び出し元 2 箇所はどちらも `window` を持つ (`for_project_item` の引数、購読は `subscribe_in` に変えられる) | `:668-690`, `:1239-1250`, `:1265`, `:1272` |
| `cx.subscribe_in(&emitter, window, \|this, _, event, window, cx\| ..)` は `Context<T>` にあり、同ファイルで `text_input` の購読に使っている。コールバックは `with_window` → `subscriber.update` 内で走り、window が無ければ黙って飛ばす | `crates/gpui/src/app/context.rs:355-384`, `crates/gpui/src/app.rs:1853-1861`, `:1251-1259` |
| `write_answer` 1 回でも `buffer.edit` → `save_buffer` の過程で `Edited` → `Saved` 等が続き、`reparse` は 1 操作につき複数回走る。テキストは最初の `Edited` で確定するので完了状態の遷移は 1 度だけ成立する。`save_buffer` は detach された非同期 | `:822-832`, `:1239-1250` |
| `render_question` の `disabled = block.answer.is_none() \|\| block.read_only.is_some()` が option 行の `on_click`/`cursor_pointer`、`Checkbox.disabled`、自由記述欄の表示とクリックを制御 | `:889`, `:974-980`, `:986`, `:1004-1025` |
| `Checkbox` は disabled で `on_click` を捨て、`cursor_not_allowed` と `element_disabled` 背景になる | `crates/ui/src/components/toggle.rs:236-240, 279` |
| ファイル書き込みは `write_answer` のみ。呼び出し元は `on_option_clicked` と `commit_editing`。`buffer.edit` の呼び出しも `write_answer` 内の 1 箇所 | `:812`, `:761`, `:769`, `:803`, `:824` (rg で全件) |
| `commit_editing` は `self.editing.take()` が `None` なら即 return。`_window` を使わず、フォーカスは触らない。呼び出し元は `on_option_clicked`、`start_editing`、`menu::Confirm`、`EditorEvent::Blurred` | `:790-804`, `:747`, `:773`, `:1082`, `:1256` |
| `cancel_editing(window, cx)` は `editing = None` にしてビューの `focus_handle` にフォーカスを戻す。呼び出し元は `menu::Cancel` のみ | `:806-810`, `:1084-1086` |
| `start_editing` は初期テキストを `parse_answer(block).other` (無ければ `""`) にし、`text_input` にフォーカスを移す。書き込みはしない | `:772-788` |
| `with_other(block, current, "")` は `Single` で `choices` を空にし `other = None` を返す → `render_answer` は `""` → `answer_line` は `[Answer]:`。つまり Other を開いて空のまま確定すると既存の選択が消える (Enter / blur の既存挙動) | `:556-565`, `:488-491`, `:523-530` |
| `IconButton::new(id, IconName)` に `.icon_size(IconSize::Small)`、`.toggle_state(bool)` (`Toggleable`)、`.selected_icon(IconName)`、`.disabled(bool)` (`Disableable`)、`.tooltip(..)` (`ButtonCommon`)、`.on_click(..)` (`Clickable`) を連鎖できる。`tooltip` は `ButtonCommon` trait のメソッドなので trait の import が要る | `crates/agentium/src/arena.rs:1565-1577`, `crates/ui/src/components/button/icon_button.rs:83, 189-207`, `crates/ui/src/components/button/button_like.rs:17, 39, 651, 658, 673, 711` |
| `IconName::Lock` / `IconName::LockOff` が存在する | `crates/icons/src/icons.rs:192-193` |
| `Tooltip::text("...")` の用例 | `crates/agentium/src/review_view.rs:712` |
| 既存 import は `ui::{ActiveTheme, Button, Checkbox, Clickable, Color, Icon, IconName, Label, LabelCommon, ToggleState, h_flex, utils::WithRemSize, v_flex}` (`ui::prelude` は使っていない)。`ButtonCommon`, `Disableable`, `IconButton`, `IconSize`, `Toggleable`, `Tooltip` を足す。同 crate の `review_view.rs:26-29` が同じ形で import している | `:12-15` |
| GPUI テストの足場: `open_questionnaire(cx, text) -> (fs, view, cx)`、`draw_window(cx)`、`ON_DISK` (1 問、選択肢 A / B / X. Other、未回答、`Single`)。テストは `view.on_option_clicked`、`view.commit_editing`、`view.text_input`、`view.is_editing`、`view.progress`、`view.question` を直接呼ぶ。外部変更は `fs.insert_file` → `run_until_parked` で `Reloaded` → `reparse` まで進む (buffer が clean のときだけ。dirty なら conflict でテキストは変わらない) | `:2015-2054`, `:1962-1967`, `:1868-1869`, `:1948-1959`, `crates/language/src/buffer.rs:1669, 1713` |
| 単一選択で選択中の option を再クリックすると解除される (`toggle_option`)。Other 単独の書き戻しは `X: text` | `:546-547`, `:504`, テスト `:1825`, `:2006` |
| `try_open` はファイル名 `*-questions.md` だけで claim する。質問 0 件のファイルもビューで開く | `:587-593` |
| `window.focus(&handle)` は `Window::focus` を即時に書き換え、observer 通知は `cx.defer` に回す (購読コールバック内から呼んで安全)。GPUI の `draw` は `Window::focus` を書き換えない。描画されなくなった Editor がフォーカスを持ったままだとキー入力は root ノードに落ちて誰にも届かない。アプリでは `Workspace::new` の `on_focus_lost` → `focus_lost_restore_target` が直前の focus path の描画済み祖先 (= ビューの `track_focus`) に戻すが、`Workspace` の無い GPUI テストでは起きない | `crates/gpui/src/window.rs:2041-2047, 2050-2084`, `crates/workspace/src/workspace.rs:1681-1687`, `:1078` |
| focus listener (Editor の `Blurred` の源) は `draw` の中でしか走らない。`window.focus(other)` から次の draw までの間、`Window::focus` は移っているのに `Blurred` は未発火 | `crates/gpui/src/window.rs:2925-2964`, `crates/gpui/src/app/context.rs:605-611`, `crates/editor/src/editor.rs:10634-10654` |
| `FocusHandle::is_focused(&self, &Window)` は `window.focus == Some(id)` の即時比較。`VisualTestContext` の `cx.update(\|window, cx\| ..)` から呼べる | `crates/gpui/src/window.rs:497-499, 605-607` |
| window 非アクティブ化時は `current_focus_path` が空扱いになり `Blurred` → `commit_editing` が走る (既存挙動) | `crates/gpui/src/window.rs:2955-2959` |
| Escape はグローバルに `menu::Cancel`、`Editor` コンテキストでは先に `editor::Cancel` が試され、取り消すものが無ければ propagate してビューの `on_action` に届く。`cancel_editing` の唯一の呼び出し元 | `assets/keymaps/default-macos.json:27, 59`, `:1084-1086` |
| `parse_answer` は raw が空か `_` だけなら未回答扱い | `:407` |
| `README.md` 先頭に `> [!IMPORTANT]` 2 行は既にある | `head -3 README.md` (2026-08-30) |

## 3. 設計

### 3.1 状態

```rust
pub struct QuestionnaireView {
    // 既存フィールド …
    locked: bool,
}

fn is_complete(&self) -> bool {
    let (answered, total) = self.progress();
    total > 0 && answered == total
}
```

`locked` の初期値は `false`。`for_project_item` 内の最初の `reparse` で決まる。

### 3.2 自動ロック / 自動解除 (`reparse`)

```rust
fn reparse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let text = self.buffer(cx).read(cx).text();
    let was_complete = self.is_complete();
    self.document = parse_questionnaire(&text);
    let now_complete = self.is_complete();
    if was_complete != now_complete {
        self.locked = now_complete;
        if self.locked {
            self.cancel_editing(window, cx);
        }
    }
    // 以降は既存のまま (markdown 同期、emit、notify)
}
```

buffer の購読は `cx.subscribe(&buffer, ..)` から `cx.subscribe_in(&buffer, window, ..)` に変え、
`this.reparse(window, cx)` を呼ぶ。`for_project_item` 末尾の `view.reparse(cx)` も `window` を渡す。

- 完了状態が変わった時だけ `locked` を書き換え、それ以外の `reparse` では手動トグルの結果を保つ。
  常時評価 (`answered == total` なら常に `locked = true`) にすると、100% でアンロックした直後に
  A → B と選び直しただけで再ロックされ、アンロックが使えない。
- 未完 → 完了でロック、完了 → 未完で解除の対称にする。片方向 (ロックのみ) だと、ロック中に
  エージェントが質問を追記して (n, n) → (n, n+1) になった時、あるいは次ステージ用にファイル全体が
  書き換わって (0, m) になった時にロックが残り、新しい質問に答えるたびに手動解除が要る。AI-DLC では
  `## Requested Changes Feedback` の追記が通常経路 (`questionnaire-view-plan.md:78`)。
- 具体的な遷移:
  - 開いた瞬間 100%: `document` が空 (`total == 0` → 未完) → (n, n) で完了 → ロック
  - 最後の 1 問に回答: (n-1, n) → (n, n) → ロック
  - 100% でアンロック → A → B: (n, n) → (n, n)、遷移なし → アンロック維持
  - 100% でアンロック → 選択解除: (n, n) → (n-1, n) → `locked = false` (変化なし) → 再選択で (n, n) → 再ロック
  - ロック中に外部追記: (n, n) → (n, n+1) → 解除 → 回答して (n+1, n+1) → 再ロック
  - 手動ロック (n-1, n) → 外部で最後の 1 問が埋まる → (n, n) → `locked = true` (同値)
  - 100% でアンロック → 外部で別の完了済みセット (n', n') に書き換わる → 遷移なし → アンロック維持
    (同一ビュー内の手動解除の継続として許容する)
  - `total == 0` は完了扱いにしない
- ロックへの遷移時は `cancel_editing` (§3.4) で入力欄を閉じる。ファイルが外から変わった後の入力なので
  確定しない。

### 3.3 ヘッダのボタン

既存の `Label` を `h_flex().gap_2().items_center()` で包み、右に置く:

```rust
IconButton::new("edit-lock", IconName::LockOff)
    .icon_size(IconSize::Small)
    .toggle_state(self.locked)
    .selected_icon(IconName::Lock)
    .disabled(total == 0)
    .tooltip(Tooltip::text(if self.locked { "Unlock answers" } else { "Lock answers" }))
    .on_click(cx.listener(|this, _, window, cx| this.toggle_lock(window, cx)))
```

```rust
fn toggle_lock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if !self.locked && self.editing.is_some() {
        if self.text_input.read(cx).text(cx).trim().is_empty() {
            self.cancel_editing(window, cx);
        } else {
            self.commit_editing(window, cx);
            self.reclaim_focus(window, cx);
        }
    }
    self.locked = !self.locked;
    cx.notify();
}
```

- ロックする方向で入力欄が開いていれば、非空なら確定、空なら破棄する。
  - 非空: ユーザー自身の入力なので保存する (外部変更による §3.2 の破棄とは区別する)。`commit_editing`
    は `locked` がまだ `false` の間に `write_answer` を通るので guard に掛からない。その結果
    (n-1, n) → (n, n) になれば `reparse` 側でも `locked = true` になるが同値。`parse_answer` が非空と
    見なす入力なら質問は回答済みになるので下向き遷移は起きない (例外は §6 の `___`)。
  - 空: 確定すると `with_other(block, current, "")` が `Single` の既存選択を消し (§2)、例えば `[Answer]: B`
    の質問で X をクリックしただけの状態から Lock を押すと B が失われ、(n, n) → (n-1, n) の下向き遷移で
    直後の `reparse` が `locked = false` に戻す。「保護」ボタンで回答が消えてロックも掛からないのは
    許容できないので `cancel_editing` にする。空入力の Enter / blur が選択を消す既存挙動は変えない。
- `render` は `let (answered, total) = self.progress();` を既に持っているので `total` はそのまま使う。
- `.disabled(total == 0)` は依頼文に無い判断 (§6)。
- ヘッダは `WithRemSize` の外なので、既存 Label と同じく rem 変更の影響を受けない。

### 3.4 書き込み抑止と入力欄の後始末

- `write_answer` の先頭に `if self.locked { return; }`。クリック (`on_option_clicked`)、Enter
  (`menu::Confirm` → `commit_editing`)、blur (`EditorEvent::Blurred` → `commit_editing`) のすべてが
  ここを通るので、書き込みはこの 1 箇所で止まる。
- `start_editing` の先頭にも `if self.locked { return; }`。書き込みはしないが、ロック中に Other の
  クリックで入力欄が開くのを防ぐ。`on_option_clicked` 自体には guard を置かない (`write_answer` と
  `start_editing` の guard で全経路が塞がる)。
- フォーカスの戻しは 1 箇所にまとめる:

  ```rust
  fn reclaim_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
      if self.text_input.focus_handle(cx).is_focused(window) {
          window.focus(&self.focus_handle, cx);
      }
  }

  fn cancel_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
      self.editing = None;
      self.reclaim_focus(window, cx);
      cx.notify();
  }
  ```

  `cancel_editing` の再フォーカスを無条件から「`text_input` が実際にフォーカスを持つとき」に変える。
  `editing.is_some()` は `text_input` がフォーカスを持つことと等価ではない: 別ペインをクリックすると
  `window.focus(other)` は即時だが `Blurred` (→ `commit_editing` → `editing = None`) は次の draw まで
  遅れるので、その間に `Reloaded` が届くと `editing.is_some()` のまま `reparse` に入る (§2)。無条件に
  戻すとユーザーが移したフォーカスを奪い返す。`menu::Cancel` 経路では Escape が `text_input` から
  dispatch されるので条件は常に真で、挙動は変わらない。
- 編集中 (`editing.is_some()`) にロックが立つ経路は 2 つ。外部書き込みによる `Reloaded` → `reparse` は
  `cancel_editing` (§3.2)、ロックボタンは空なら `cancel_editing`、非空なら `commit_editing` +
  `reclaim_focus` (§3.3)。`commit_editing` 自身はフォーカスを触らないので、ボタン経路にも
  `reclaim_focus` が要る。どちらの経路も `editing` を `None` にし、`text_input` にフォーカスがあれば
  ビューへ戻すので、描画されない Editor にフォーカスが残る状態を作らず、その後 `Blurred` が来ても
  `commit_editing` は `take()` が `None` で空振りする。`Workspace::on_focus_lost` の救済 (§2) には
  依存しない。

### 3.5 見た目 (`render_question`)

```rust
let disabled = block.answer.is_none() || block.read_only.is_some();
let interactive = !disabled && !self.locked;
```

- option 行の `cursor_pointer` / `on_click`: `.when(interactive, ..)` に変更。
- `Checkbox::new(..).disabled(!interactive)`。ロック中は `element_disabled` 背景と `cursor_not_allowed`
  になり、クリックは `Checkbox` 側で捨てられる。
- 自由記述欄: 表示条件 `show_free_text && !disabled` は変えない (ロック中も "Other: xxx" や Feedback の
  本文を表示し続ける)。`is_editing(index)` は `editing = None` により false。非編集時の分岐は:
  - `interactive` なら現状通り (本文または placeholder "Click to type an answer"、クリックで `start_editing`)
  - そうでなければ本文があるときだけ `div().text_size(rems(1.0)).text_color(colors.text_muted)` で表示、
    `cursor_pointer` / `on_click` なし。本文が空なら何も描かない (placeholder はロック中に出さない)
- 質問右上の raw 回答表示 (`:925-939`) と `notice` は変更しない。ロックは警告ではなくヘッダの状態で示す。

### 3.6 変更しないもの

`QuestionnaireItem`、`Item` 実装、`OpenAsText`、フォントサイズ処理、パーサ・シリアライザ、
`commit_editing` の中身。

## 4. 実装ステップ

各ステップは単体でビルドとテストが通る状態で終える。テストは Red → Green の順。

0. `README.md` 先頭の `> [!IMPORTANT]` 2 行を確認する (HEAD に既にある)。
1. `locked` フィールド、`is_complete`、`reparse(window, cx)` 化と `subscribe_in`、遷移検出、
   `reclaim_focus` と `cancel_editing` の条件化、`write_answer` / `start_editing` の guard。
   テスト `lock_engages_at_full_completion`、`opens_locked_when_already_complete`、`appended_question_unlocks`、
   `lock_during_editing_discards_pending_text`。
2. `toggle_lock` とヘッダの `IconButton`。import 追加。テスト `lock_toggle_reenables_writes`、
   `manual_lock_commits_pending_text`、`manual_lock_with_empty_input_keeps_answer`。
3. `render_question` の `interactive` 分岐と自由記述欄の表示。既存テストの `draw_window` で panic しないこと。
4. テスト `empty_questionnaire_never_locks`。
5. `./script/clippy` と `cargo test -p agentium questionnaire` を通す。`cargo run -p agentium` で手動シナリオ。

## 5. テスト計画

GPUI テスト (`#[gpui::test]`、`open_questionnaire` を使う。`locked` は同一モジュールの private field
としてテストから直接読む)。2 問の fixture `TWO_ON_DISK` を追加する (`ON_DISK` の末尾 `[Answer]:\n` に
続けて空行 1 つ、`## Q2. Which?\n\nA. Foo\nB. Bar\n\n[Answer]:\n`)。`fs.insert_file` で書く内容は
fixture と同一バイト列にする。`write_answer` の保存は非同期なので、`toggle_lock` / `on_option_clicked` /
`commit_editing` の後は必ず `run_until_parked` してから `fs.load` する。フォーカスの assert は
`cx.update(|window, cx| view.read(cx).focus_handle.is_focused(window))`。

- `lock_engages_at_full_completion`: `ON_DISK` (0/1) で開く → `locked == false` →
  `on_option_clicked(0, 1)` → `progress() == (1, 1)` かつ `locked == true` → `on_option_clicked(0, 0)` →
  ファイルは `[Answer]: B` のまま (ロック中は書かない)。
- `opens_locked_when_already_complete`: `ON_DISK.replace("[Answer]:", "[Answer]: A")` で開く →
  `run_until_parked` 直後に `locked == true`。
- `appended_question_unlocks`: 上の状態 (ロック中) から `fs.insert_file` で Q2 (未回答) を追記 →
  `progress() == (1, 2)` かつ `locked == false` → `on_option_clicked(1, 0)` → ファイルの Q2 が
  `[Answer]: A`、`progress() == (2, 2)`、`locked == true`。
- `lock_toggle_reenables_writes`: `opens_locked_when_already_complete` の状態から `toggle_lock` →
  `locked == false` → `on_option_clicked(0, 1)` → ファイルが `[Answer]: B`、(1, 1) のまま遷移なしなので
  `locked == false` を維持 → `on_option_clicked(0, 1)` (再クリックで解除、0/1) → `locked == false` →
  `on_option_clicked(0, 0)` (1/1) → `locked == true` (再ロック)。
- `manual_lock_commits_pending_text`: `TWO_ON_DISK` で開く → `on_option_clicked(0, 2)` (Other →
  入力欄) → `text_input.set_text("draft")` → `toggle_lock` → ファイルの Q1 が `[Answer]: X: draft`、
  `!is_editing(0)`、ビューの `focus_handle.is_focused(window)`、`progress() == (1, 2)`、`locked == true`
  (手動) → `on_option_clicked(1, 0)` → ファイル不変 (ロック中) → `toggle_lock` → `on_option_clicked(1, 0)`
  → Q2 が `[Answer]: A`、`locked == true` (2/2 で自動)。
- `manual_lock_with_empty_input_keeps_answer`: `ON_DISK.replace("[Answer]:", "[Answer]: B")` で開く →
  `locked == true` → `toggle_lock` (解除) → `on_option_clicked(0, 2)` (Other → 入力欄、テキスト `""`) →
  `is_editing(0)` → `toggle_lock` → ファイルは `[Answer]: B` のまま、`!is_editing(0)`、
  `focus_handle.is_focused(window)`、`locked == true`。
- `lock_during_editing_discards_pending_text`: `ON_DISK` で開く → `on_option_clicked(0, 2)` (Other →
  入力欄) → `is_editing(0)` → `fs.insert_file` で `[Answer]: A` を書く → `locked == true`、
  `!is_editing(0)`、`focus_handle.is_focused(window)` → `text_input.set_text("x")` して `commit_editing`
  → ファイルは `[Answer]: A` のまま。
- `empty_questionnaire_never_locks`: `"# notes\n"` を `*-questions.md` として開く → `progress() == (0, 0)`、
  `locked == false`。`draw_window` で panic しない (ボタンは disabled 描画)。

手動シナリオ (4 と 6 の「別ターミナル」は同一ウィンドウ内のターミナルペイン。別ウィンドウに切り替えると
window 非アクティブ化で Editor が blur し、既存挙動の `commit_editing` が先に走る):

1. 回答済みの質問票を Cmd+P で開く → ヘッダの Lock アイコンが選択状態、Checkbox が disabled 表示、
   クリックしても変わらない。
2. Lock を押して解除 → クリックで回答が変わりファイルが更新される。1 問を解除して再回答すると再ロック。
3. 未回答 1 問を残した質問票で最後の 1 問に回答 → その瞬間ロック。
4. Other の入力欄を開いたまま同一ウィンドウのターミナルペインから `[Answer]:` を埋める → 入力欄が閉じて
   ロック。ビューに戻って Enter を押しても上書きされない。フォーカスはビューにある (cmd-+ が効く)。
5. Other に入力してから Lock を押す → 入力が保存されてロック。Other を開いただけで空のまま Lock を
   押す → 元の回答が残ってロック。
6. ロック中に同一ウィンドウのターミナルペインから質問を 1 つ追記 → 解除され、答えると再ロック。
7. cmd-+ で拡大してもヘッダのボタンサイズは変わらない (既存 Label と同じ)。

## 6. リスクと未決事項

- 依頼文は「100% になった瞬間に ON」だけで、未完に戻った時の挙動は書かれていない。本プランは
  完了 → 未完で自動解除する対称ルールを採る (§3.2 の追記ケースのため)。片方向にしたい場合は
  `self.locked = now_complete` を `if now_complete { self.locked = true }` に変えるだけだが、
  追記後は手動解除が必要になる。
- `.disabled(total == 0)` も依頼文に無い。質問 0 件でロックする対象が無いので押せなくしているだけで、
  外しても動作に影響はない。
- 空入力の Other を Enter / blur で確定すると `Single` の既存選択が消える既存挙動はそのまま。Lock ボタン
  経路だけ `cancel_editing` で回避する (§3.3)。
- 文字トークンを使わない質問 (`Feedback`、bare token の `Single`) で `___` だけを入力して Lock を押すと、
  `render_answer` がそのまま書いた `[Answer]: ___` を `parse_answer` が未回答と見なし、下向き遷移で
  Lock が取り消される。下線だけを入力する操作は現実的でないので対処しない (文字トークン付き質問は
  `X: ___` になり該当しない)。
- ロックは View のメモリ状態。タブを閉じて開き直すと 100% なら再ロックされるが、アンロック状態は
  残らない (仕様上は問題なし)。
- 同じファイルを 2 つの pane で開くと各 View が独立にロック状態を持つ。完了状態の遷移は両方の
  `reparse` で同じに見えるので、遷移時には揃う。手動トグルは片方だけに効く。
- Checkbox の disabled 表示が「ロック中」と「`[Answer]:` 行なし / 読み取り専用」で同じ見え方になる。
  ヘッダの Lock アイコンで区別する。必要になれば後でロック中のカード枠色を変える。
- `reparse` が `Window` を要求するようになるので、将来 `Window` の無い文脈から再解析したくなった時は
  フォーカス処理を分離する必要がある。現時点の呼び出し元 2 箇所はどちらも `window` を持つ。

## 7. 見積り

- 本体: 70〜90 行 (フィールド、`is_complete`、`toggle_lock`、`reclaim_focus`、guard、ボタン、render 分岐、
  `subscribe_in` 化)
- テスト: 150〜190 行 (8 本 + fixture)
- 変更ファイルは `crates/agentium/src/questionnaire_view.rs` のみ
