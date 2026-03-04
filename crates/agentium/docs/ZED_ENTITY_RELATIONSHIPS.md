# Zed エンティティ関連図 — Project / Worktree / File / Git

**調査日**: 2026-03-04

---

## 1. 全体アーキテクチャ概要

```
┌─── Entity<Project> ──────────────────────────────────────────────────────────┐
│                                                                               │
│  ┌── Entity<WorktreeStore> ─────────────────────────────────────────────┐     │
│  │  worktrees: Vec<WorktreeHandle>                                      │     │
│  │    ├── Entity<Worktree::Local(LocalWorktree)>                       │     │
│  │    │     └── snapshot: LocalSnapshot                                 │     │
│  │    │           ├── snapshot: Snapshot                                │     │
│  │    │           │    ├── entries_by_path: SumTree<Entry>             │     │
│  │    │           │    └── entries_by_id: SumTree<PathEntry>           │     │
│  │    │           └── git_repositories: TreeMap<ProjectEntryId,        │     │
│  │    │                                       LocalRepositoryEntry>    │     │
│  │    └── Entity<Worktree::Remote(RemoteWorktree)>                     │     │
│  │          └── snapshot: Snapshot                                      │     │
│  └─────────────────────────────────────────────────────────────────────┘     │
│                                                                               │
│  ┌── Entity<GitStore> ──────────────────────────────────────────────────┐     │
│  │  repositories: HashMap<RepositoryId, Entity<Repository>>             │     │
│  │    └── Repository                                                    │     │
│  │          ├── snapshot: RepositorySnapshot                            │     │
│  │          │    ├── statuses_by_path: SumTree<StatusEntry>            │     │
│  │          │    ├── branch: Option<Branch>                             │     │
│  │          │    ├── head_commit: Option<CommitDetails>                │     │
│  │          │    ├── merge: MergeDetails                               │     │
│  │          │    └── stash_entries: GitStash                           │     │
│  │          └── paths_needing_status_update: BTreeSet<RepoPath>        │     │
│  │  worktree_ids: HashMap<RepositoryId, HashSet<WorktreeId>>           │     │
│  │  diffs: HashMap<BufferId, Entity<BufferGitState>>                   │     │
│  └─────────────────────────────────────────────────────────────────────┘     │
│                                                                               │
│  ┌── Entity<BufferStore> ───────────────────────────────────────────────┐     │
│  │  opened_buffers: HashMap<BufferId, OpenBuffer>                       │     │
│  │  path_to_buffer_id: HashMap<ProjectPath, BufferId>                  │     │
│  │  loading_buffers: HashMap<ProjectPath, Shared<Task<...>>>           │     │
│  │  worktree_store: Entity<WorktreeStore>                              │     │
│  └─────────────────────────────────────────────────────────────────────┘     │
│                                                                               │
│  その他: Entity<LspStore>, Entity<TaskStore>, Entity<DapStore>, ...         │
│                                                                               │
└───────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. 主要エンティティと構造体

### 2.1 Project

**ファイル**: `crates/project/src/project.rs:206-245`

Project はセマンティクス対応のコンテナで、複数のワークツリーとその関連サービスを統合管理する。

```rust
pub struct Project {
    worktree_store: Entity<WorktreeStore>,    // ワークツリー管理
    buffer_store: Entity<BufferStore>,         // バッファ管理
    git_store: Entity<GitStore>,               // Git リポジトリ管理
    lsp_store: Entity<LspStore>,               // LSP 管理
    task_store: Entity<TaskStore>,             // タスク管理
    active_entry: Option<ProjectEntryId>,      // 現在アクティブなファイル
    // ... (他多数)
}
```

**設計ポイント**: Project は各種ストアを `Entity<T>` として保持し、それぞれが独立したエンティティとして振る舞う。Project 自体にはデータを直接格納せず、ストアを通じてアクセスする。

### 2.2 WorktreeStore

**ファイル**: `crates/project/src/worktree_store.rs:67-79`

```rust
pub struct WorktreeStore {
    worktrees: Vec<WorktreeHandle>,            // Worktree エンティティのハンドル群
    next_entry_id: Arc<AtomicUsize>,
    next_worktree_id: WorktreeIdCounter,
    scanning_enabled: bool,
    // ...
}
```

**主要メソッド**:
- `worktrees()` — 全ワークツリーのイテレータ
- `visible_worktrees(cx)` — ユーザーに見えるワークツリー
- `worktree_for_id(id, cx)` — ID による検索
- `worktree_for_entry(entry_id, cx)` — エントリ ID からワークツリーを逆引き
- `entry_for_id()` / `entry_for_path()` — エントリ検索

### 2.3 Worktree

**ファイル**: `crates/worktree/src/worktree.rs:90-164`

```rust
pub enum Worktree {
    Local(LocalWorktree),
    Remote(RemoteWorktree),
}

pub struct LocalWorktree {
    snapshot: LocalSnapshot,                    // ファイルツリーのスナップショット
    fs: Arc<dyn Fs>,                           // ファイルシステム抽象
    scan_requests_tx: channel::Sender<ScanRequest>, // バックグラウンドスキャナーへの指示
    // ...
}

pub struct RemoteWorktree {
    snapshot: Snapshot,                         // リモートから受信したスナップショット
    project_id: u64,
    client: AnyProtoClient,
    // ...
}
```

### 2.4 Snapshot / LocalSnapshot

**ファイル**: `crates/worktree/src/worktree.rs:166-253`

```rust
pub struct Snapshot {
    id: WorktreeId,
    abs_path: Arc<SanitizedPath>,              // ルートディレクトリの絶対パス
    entries_by_path: SumTree<Entry>,           // パスでインデックスされたエントリ群
    entries_by_id: SumTree<PathEntry>,         // ID でインデックスされたエントリ群
    scan_id: usize,                            // 現在のスキャン番号
    completed_scan_id: usize,                  // 最後に完了したスキャン
    // ...
}

pub struct LocalSnapshot {
    snapshot: Snapshot,                        // ベーススナップショット (Deref で透過的にアクセス)
    git_repositories: TreeMap<ProjectEntryId, LocalRepositoryEntry>,  // 検出された Git リポジトリ
    ignores_by_parent_abs_path: HashMap<Arc<Path>, (Arc<Gitignore>, bool)>, // .gitignore キャッシュ
    // ...
}
```

**SumTree**: Zed 独自の永続的 B-tree。パス順序でのイテレーション、範囲検索、集計 (Summary) を O(log n) で実現する。

### 2.5 Entry

**ファイル**: `crates/worktree/src/worktree.rs:3348-3388`

```rust
pub struct Entry {
    pub id: ProjectEntryId,                    // ワークツリー内でユニークな ID
    pub kind: EntryKind,                       // File / Dir / PendingDir / UnloadedDir
    pub path: Arc<RelPath>,                    // ルートからの相対パス
    pub inode: u64,
    pub mtime: Option<MTime>,
    pub is_ignored: bool,                      // .gitignore で無視されているか
    pub is_hidden: bool,
    pub is_external: bool,                     // シンボリックリンク経由の外部ファイル
    pub is_private: bool,                      // .env 等のプライベートファイル
    pub size: u64,
    pub char_bag: CharBag,                     // ファジー検索用
    // ...
}
```

**重要**: Entry は **Git ステータスを直接保持しない**。`is_ignored` フラグのみ .gitignore の判定結果を格納する。

---

## 3. ID 体系

```
WorktreeId(usize)         — ワークツリーの識別子 (settings クレートで定義)
ProjectEntryId(usize)     — ワークツリー内エントリの識別子
BufferId                   — バッファの識別子
RepositoryId(u64)         — Git リポジトリの識別子

ProjectPath {              — プロジェクト横断のファイル識別子
    worktree_id: WorktreeId,
    path: Arc<RelPath>,
}
```

**パス解決フロー**:
```
ProjectPath(worktree_id, path)
  → WorktreeStore.worktree_for_id(worktree_id)
    → Worktree.snapshot.entries_by_path.get(path)
      → Entry { id, kind, path, ... }
```

---

## 4. Git 連携アーキテクチャ

### 4.1 Git リポジトリの発見 (Worktree 層)

**ファイル**: `crates/worktree/src/worktree.rs:271-293`

ファイルスキャン中に `.git` ディレクトリを発見すると `LocalRepositoryEntry` を作成:

```rust
struct LocalRepositoryEntry {
    work_directory_id: ProjectEntryId,         // リポジトリルートの Entry ID
    work_directory: WorkDirectory,             // InProject | AboveProject
    work_directory_abs_path: Arc<Path>,
    dot_git_abs_path: Arc<Path>,               // .git の絶対パス
    common_dir_abs_path: Arc<Path>,            // サブモジュール用
    repository_dir_abs_path: Arc<Path>,
}
```

```rust
enum WorkDirectory {
    InProject {
        relative_path: Arc<RelPath>,           // .git がワークツリー内にある場合
    },
    AboveProject {
        absolute_path: Arc<Path>,              // .git がワークツリーの上位にある場合
        location_in_repo: Arc<Path>,           // リポ内でのワークツリーの位置
    },
}
```

**1つのワークツリーに複数の Git リポジトリが存在しうる** (ネストされたリポジトリや、リポジトリのルートがワークツリーの上位にあるケース)。

### 4.2 Git ステータス管理 (GitStore 層)

**ファイル**: `crates/project/src/git_store.rs:90-103, 266-278, 299-323`

```rust
pub struct GitStore {
    repositories: HashMap<RepositoryId, Entity<Repository>>,
    worktree_ids: HashMap<RepositoryId, HashSet<WorktreeId>>,
    diffs: HashMap<BufferId, Entity<BufferGitState>>,
    // ...
}

pub struct Repository {
    snapshot: RepositorySnapshot,
    paths_needing_status_update: BTreeSet<RepoPath>,
    job_sender: mpsc::UnboundedSender<GitJob>,
    // ...
}

pub struct RepositorySnapshot {
    pub id: RepositoryId,
    pub statuses_by_path: SumTree<StatusEntry>,     // ★ 全ファイルの Git ステータス
    pub branch: Option<Branch>,                      // 現在のブランチ
    pub head_commit: Option<CommitDetails>,          // HEAD コミット
    pub merge: MergeDetails,                         // マージ中の状態
    pub stash_entries: GitStash,                     // スタッシュ
    pub remote_origin_url: Option<String>,
    pub remote_upstream_url: Option<String>,
    // ...
}
```

### 4.3 Git ステータスの型

**ファイル**: `crates/git/src/status.rs`

```rust
pub enum FileStatus {
    Untracked,                                 // 未追跡
    Ignored,                                   // 無視
    Unmerged(UnmergedStatus),                  // マージコンフリクト中
    Tracked(TrackedStatus),                    // 追跡対象
}

pub struct TrackedStatus {
    pub index_status: StatusCode,              // ステージングエリア (index) のステータス
    pub worktree_status: StatusCode,           // ワーキングツリーのステータス
}

pub enum StatusCode {
    Modified, TypeChanged, Added, Deleted, Renamed, Copied, Unmodified,
}
```

`StatusEntry` は SumTree のアイテムで、パスとステータスのペア:

```rust
pub struct StatusEntry {
    pub repo_path: RepoPath,                   // リポジトリルートからの相対パス
    pub status: FileStatus,
}
```

### 4.4 Entry と Git ステータスの結合 (GitTraversal)

**ファイル**: `crates/project/src/git_store/git_traversal.rs`

**Entry は Git ステータスを直接保持しない**。代わりに `GitTraversal` がワークツリーのエントリとリポジトリのステータスをパスベースで動的に結合する:

```rust
pub struct GitTraversal<'a> {
    traversal: Traversal<'a>,                  // Entry の走査
    repo_root_to_snapshot: BTreeMap<&'a Path, &'a RepositorySnapshot>,
    // ...
}

pub struct GitEntryRef<'a> {
    pub entry: &'a Entry,                      // ファイルシステムエントリ
    pub git_summary: GitSummary,               // 結合された Git ステータス
}
```

**結合の流れ**:
1. `GitTraversal` が `entries_by_path` を走査
2. 各エントリのパスから対応する Git リポジトリを特定
3. リポジトリの `statuses_by_path` (SumTree) からそのパスのステータスを検索
4. ファイル → 直接 `FileStatus` を取得
5. ディレクトリ → 子エントリのステータスを集計して `GitSummary` を算出

```rust
pub struct GitSummary {
    pub index: TrackedSummary,                 // index 変更の集計
    pub worktree: TrackedSummary,              // worktree 変更の集計
    pub conflict: usize,
    pub untracked: usize,
    pub count: usize,
}
```

---

## 5. Buffer と Git Diff の関係

### 5.1 Buffer

**ファイル**: `crates/language/src/buffer.rs`

Buffer はエディタで開かれたファイルの内容を管理する。`language::File` トレイトを通じてワークツリーのエントリと関連付けられる。

```rust
// language::File トレイト (buffer.rs:385)
pub trait File: Send + Sync + Any {
    fn as_local(&self) -> Option<&dyn LocalFile>;
    fn disk_state(&self) -> DiskState;
    fn path(&self) -> &Arc<RelPath>;           // ワークツリー内の相対パス
    fn full_path(&self, cx: &App) -> PathBuf;
    fn worktree_id(&self, cx: &App) -> WorktreeId;
    // ...
}

// worktree::File 構造体 (worktree.rs:3195-3202)
pub struct File {
    pub worktree: Entity<Worktree>,            // 所属ワークツリーへの参照
    pub path: Arc<RelPath>,
    pub disk_state: DiskState,
    pub entry_id: Option<ProjectEntryId>,      // 対応する Entry の ID
    pub is_local: bool,
    pub is_private: bool,
}
```

### 5.2 BufferStore

**ファイル**: `crates/project/src/buffer_store.rs:32-43`

```rust
pub struct BufferStore {
    opened_buffers: HashMap<BufferId, OpenBuffer>,
    path_to_buffer_id: HashMap<ProjectPath, BufferId>,  // ProjectPath → BufferId の逆引き
    worktree_store: Entity<WorktreeStore>,
    // ...
}
```

### 5.3 BufferGitState と BufferDiff

**ファイル**: `crates/project/src/git_store.rs:111-140`, `crates/buffer_diff/src/buffer_diff.rs:24-60`

GitStore はバッファごとに `BufferGitState` を管理し、行レベルの diff を計算する:

```rust
// GitStore の diffs フィールド
diffs: HashMap<BufferId, Entity<BufferGitState>>

struct BufferGitState {
    unstaged_diff: Option<WeakEntity<BufferDiff>>,     // HEAD vs worktree
    uncommitted_diff: Option<WeakEntity<BufferDiff>>,  // HEAD vs index+worktree
    head_text: Option<Arc<str>>,                        // HEAD でのファイル内容
    index_text: Option<Arc<str>>,                       // index でのファイル内容
    conflict_set: Option<WeakEntity<ConflictSet>>,     // コンフリクトマーカー
    // ...
}

// バッファの行レベル diff
pub struct BufferDiff {
    pub buffer_id: BufferId,
    inner: BufferDiffInner<Entity<language::Buffer>>,
}

struct BufferDiffInner<BaseText> {
    hunks: SumTree<InternalDiffHunk>,          // diff ハンク (追加/削除/変更の範囲)
    base_text: BaseText,                       // 比較元テキスト (HEAD or index)
    buffer_snapshot: text::BufferSnapshot,     // 現在のバッファ内容
    // ...
}
```

---

## 6. データフロー図

### 6.1 ファイルシステム → Entry の更新

```
ファイルシステム変更 (inotify/FSEvents)
  → BackgroundScanner (バックグラウンドスレッド)
    → LocalSnapshot.entries_by_path を更新
      → Worktree が cx.notify() を発行
        → UI が再レンダリング
```

### 6.2 Git ステータスの更新

```
.git/index や作業ファイルの変更
  → BackgroundScanner が .git ディレクトリの変更を検知
    → GitStore の Repository に通知
      → Repository がバックグラウンドで git status を実行
        → statuses_by_path (SumTree<StatusEntry>) を更新
          → cx.notify() で UI に通知
```

### 6.3 Buffer の Git Diff 更新

```
バッファの内容変更 or Git ステータス更新
  → Project.buffers_needing_diff に追加 (debounced)
    → GitStore.diff_bases_changed()
      → BufferGitState が head_text / index_text を更新
        → BufferDiff がバックグラウンドで diff を再計算
          → hunks (SumTree<InternalDiffHunk>) を更新
            → エディタのガター (差分マーク) が更新
```

### 6.4 UI でのステータス表示フロー

```
プロジェクトパネル (ファイルツリー):
  WorktreeStore.worktrees()
    → GitTraversal が Entry + RepositorySnapshot を結合
      → GitEntryRef { entry, git_summary } をイテレート
        → ファイル名の色を git_summary に基づいて変更

エディタ (ガター差分マーク):
  BufferStore.opened_buffers[buffer_id]
    → GitStore.diffs[buffer_id] → BufferGitState
      → BufferDiff.hunks をイテレート
        → ガターに追加/変更/削除のマークを表示
```

---

## 7. 関連図 (ER 図風)

```
Project ─────────────────────────────────────────────────────────────
 │
 ├── 1:1 ── WorktreeStore
 │            │
 │            └── 1:N ── Entity<Worktree>
 │                         │
 │                         ├── has ── Snapshot
 │                         │           ├── entries_by_path: SumTree<Entry>
 │                         │           └── entries_by_id: SumTree<PathEntry>
 │                         │
 │                         └── has ── LocalSnapshot (Local のみ)
 │                                     └── git_repositories: TreeMap<LocalRepositoryEntry>
 │                                          │
 │                                          └── 発見された .git の位置情報
 │
 ├── 1:1 ── GitStore
 │            │
 │            ├── 1:N ── Entity<Repository>
 │            │           ├── snapshot: RepositorySnapshot
 │            │           │   ├── statuses_by_path: SumTree<StatusEntry>
 │            │           │   ├── branch, head_commit, merge, stash
 │            │           │   └── remote_origin_url, remote_upstream_url
 │            │           └── paths_needing_status_update
 │            │
 │            ├── worktree_ids: RepositoryId → Set<WorktreeId>
 │            │   (Repository と Worktree の N:M 関係)
 │            │
 │            └── diffs: BufferId → Entity<BufferGitState>
 │                        ├── unstaged_diff → Entity<BufferDiff>
 │                        ├── uncommitted_diff → Entity<BufferDiff>
 │                        ├── head_text, index_text
 │                        └── conflict_set
 │
 └── 1:1 ── BufferStore
              │
              ├── opened_buffers: BufferId → OpenBuffer
              │                    └── Entity<Buffer>
              │                         ├── text (rope)
              │                         └── file: Box<dyn File>
              │                              ├── worktree: Entity<Worktree>
              │                              ├── path: Arc<RelPath>
              │                              └── entry_id: ProjectEntryId
              │
              └── path_to_buffer_id: ProjectPath → BufferId
```

---

## 8. 設計上の重要なポイント

### 8.1 Entry に Git ステータスが格納されない理由

Entry はファイルシステムのスナップショットに属し、Git ステータスとは独立してスキャン・更新される。Git ステータスは `RepositorySnapshot.statuses_by_path` に別管理し、`GitTraversal` で動的に結合する設計になっている。

**利点**:
- ファイルスキャンと Git ステータス更新を独立して非同期実行できる
- Git リポジトリが存在しないワークツリーでも Entry は正常に機能する
- ネストされた複数 Git リポジトリの扱いが容易

### 8.2 Worktree と Repository の N:M 関係

`GitStore.worktree_ids` は `RepositoryId → HashSet<WorktreeId>` のマッピングを持つ。

- 1 つの Worktree が複数の Git リポジトリを含む場合がある (ネストされたリポジトリ)
- 1 つの Git リポジトリが複数の Worktree にまたがる場合がある (モノレポでサブディレクトリを個別に開いた場合)
- `WorkDirectory::AboveProject` により、ワークツリーのルートより上位に .git がある場合にも対応

### 8.3 2 層の diff 概念

| 概念 | 格納場所 | 粒度 | 用途 |
|---|---|---|---|
| **ファイルステータス** | `RepositorySnapshot.statuses_by_path` | ファイル単位 | プロジェクトパネルのファイル色分け、Git パネル |
| **バッファ diff** | `BufferGitState` → `BufferDiff.hunks` | 行単位 | エディタガターの差分マーク、インライン diff |

### 8.4 SumTree の活用

Entry もステータスも `SumTree` で管理されている。SumTree は各ノードに `Summary` を集計するため:

- ディレクトリの Git ステータス集計 (`GitSummary`) を O(log n) で取得可能
- パス範囲のエントリ検索が効率的
- スナップショットの差分更新 (structural sharing) が可能

---

## 9. 主要ファイルパス一覧

| 対象 | ファイルパス |
|---|---|
| Project 構造体 | `crates/project/src/project.rs:206-245` |
| WorktreeStore | `crates/project/src/worktree_store.rs:67-79` |
| Worktree / LocalWorktree / RemoteWorktree | `crates/worktree/src/worktree.rs:90-164` |
| Snapshot / LocalSnapshot | `crates/worktree/src/worktree.rs:166-253` |
| Entry | `crates/worktree/src/worktree.rs:3348-3388` |
| File 構造体 | `crates/worktree/src/worktree.rs:3195-3202` |
| File トレイト | `crates/language/src/buffer.rs:385` |
| LocalRepositoryEntry | `crates/worktree/src/worktree.rs:271-293` |
| GitStore | `crates/project/src/git_store.rs:90-103` |
| Repository / RepositorySnapshot | `crates/project/src/git_store.rs:266-323` |
| BufferGitState | `crates/project/src/git_store.rs:111-140` |
| BufferDiff | `crates/buffer_diff/src/buffer_diff.rs:24-60` |
| GitTraversal | `crates/project/src/git_store/git_traversal.rs` |
| FileStatus / StatusCode | `crates/git/src/status.rs` |
| StatusEntry | `crates/project/src/git_store.rs:192-228` |
| ProjectPath | `crates/project/src/project.rs:411-414` |
| ProjectEntryId | `crates/worktree/src/worktree.rs:5842` |
| WorktreeId | `crates/settings/src/settings.rs:86` |
| BufferStore | `crates/project/src/buffer_store.rs:32-43` |
