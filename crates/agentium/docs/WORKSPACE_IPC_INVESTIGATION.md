# Agentium: 起動中のプロセスに新しいワークスペースを開く — 調査レポート

**調査日**: 2026-03-02
**対象コミット**: main ブランチ
**質問**: `agentium -p project` で起動中のプロセスに新しいワークスペースを開くことは可能か

---

## 結論

**現状では不可能**だが、**実装は十分に実現可能**。Zed本体に成熟したIPCメカニズムが既に存在しており、Agentiumに移植することで実現できる。

---

## 1. Agentiumの現状

### 1.1 CLI引数の処理

`crates/agentium/src/main.rs:8` で唯一の引数処理:

```rust
if std::env::args().any(|arg| arg == "--printenv") {
    util::shell_env::print_env();
    return;
}
```

`-p` や `--project` 引数は**一切存在しない**。`clap`等のCLI引数パーサーも使っていない。

### 1.2 プロジェクト/ワークスペースの初期化

`main.rs:89-105` で常にカレントディレクトリをプロジェクトルートとして使用:

```rust
let project = project::Project::local(
    client_for_window, node_runtime_for_window,
    user_store_for_window, languages_for_window,
    fs_for_window, None,
    project::LocalProjectFlags::default(), cx,
);
if let Ok(cwd) = std::env::current_dir() {
    project.update(cx, |project, cx| {
        project.find_or_create_worktree(&cwd, true, cx)
    }).detach_and_log_err(cx);
}
```

### 1.3 IPC/ソケットメカニズム

**存在しない**。Agentiumはスタンドアロンの単一プロセスアプリケーション。外部からの通信手段がない。

### 1.4 ウィンドウ

`main.rs:79` で単一ウィンドウのみ作成。複数ウィンドウの管理や、既存インスタンス検出の仕組みもない。

---

## 2. Zed本体のIPCメカニズム（参考実装）

Zed CLIには、起動中のプロセスに新しいパスを開かせる成熟したIPCが存在する。

### 2.1 アーキテクチャ概要

```
[zed CLI]                              [Running Zed Instance]
    |                                         |
    |-- IpcOneShotServer作成 ------>          |
    |   (zed-cli://{random_name})             |
    |                                         |
    |-- Unix socket に URL 送信 ---> zed-{channel}.sock で受信
    |                                         |
    |                              <--- IPC接続でhandshake
    |                                         |
    |-- CliRequest::Open 送信 --->   open_workspaces() 呼出
    |                                         |
    |                              <--- CliResponse::Exit
    |-- 終了                                  |
```

### 2.2 Linux/FreeBSDでの既存インスタンス検出

`crates/zed/src/zed/open_listener.rs:277-299`:

```rust
pub fn listen_for_cli_connections(opener: OpenListener) -> Result<()> {
    let sock_path = paths::data_dir().join(format!("zed-{}.sock", *RELEASE_CHANNEL_NAME));
    // 既存ソケットのプロセスが死んでいれば削除
    if let Err(e) = UnixDatagram::unbound()?.connect(&sock_path)
        && e.kind() == std::io::ErrorKind::ConnectionRefused
    {
        std::fs::remove_file(&sock_path)?;
    }
    let listener = UnixDatagram::bind(&sock_path)?;
    thread::spawn(move || {
        while let Ok(len) = listener.recv(&mut buf) {
            opener.open(RawOpenRequest { urls: vec![url], ..Default::default() });
        }
    });
    Ok(())
}
```

### 2.3 CLI側のインスタンス検出とIPC URL送信

`crates/cli/src/main.rs:840-856`:

```rust
fn launch(&self, ipc_url: String, user_data_dir: Option<&str>) -> anyhow::Result<()> {
    let sock_path = data_dir.join(format!("zed-{}.sock", *RELEASE_CHANNEL_NAME));
    let sock = UnixDatagram::unbound()?;
    if sock.connect(&sock_path).is_err() {
        // インスタンスが無い → 新規起動
        self.boot_background(ipc_url, user_data_dir)?;
    } else {
        // インスタンスが既に起動中 → IPC URL をソケットで送信
        sock.send(ipc_url.as_bytes())?;
    }
    Ok(())
}
```

### 2.4 CliRequest データ構造

`crates/cli/src/cli.rs:12-25`:

```rust
pub enum CliRequest {
    Open {
        paths: Vec<String>,
        urls: Vec<String>,
        diff_paths: Vec<[String; 2]>,
        diff_all: bool,
        wsl: Option<String>,
        wait: bool,
        open_new_workspace: Option<bool>,
        reuse: bool,
        env: Option<HashMap<String, String>>,
        user_data_dir: Option<String>,
    },
}
```

---

## 3. 実装方法の選択肢

### 方法A: Zed IPC メカニズムの移植（推奨）

**概要**: Zedの `listen_for_cli_connections` + `OpenListener` パターンをAgentiumに移植。

**必要な変更**:

1. **Agentium用CLIラッパー作成** または `main.rs` にclap引数パーサー追加
   - `-p <path>` でプロジェクトパスを受け取る
   - `--new` で強制的に新ワークスペース追加

2. **Unix socketリスナーの追加** (`agentium-{channel}.sock`)
   - 起動時にソケットをbind
   - 既にソケットが存在すれば既存インスタンスに委譲

3. **AgentiumApp にワークスペース追加APIを公開**
   - 既に `add_workspace()` メソッドが存在（`agentium.rs:60-76`）
   - これを外部IPC経由で呼べるようにする
   - プロジェクトパスを指定して worktree を追加

4. **IPCハンドラーの実装**
   - `ipc-channel` クレートを使用（既にZed依存に存在）
   - `CliRequest::Open` を受信→ `AgentiumApp::add_workspace_with_path()` を呼出

**工数見積り**: Medium（2-4日）
**再利用率**: 70-80%（Zedのopen_listener.rsから大部分を流用可能）

### 方法B: D-Bus / プラットフォーム固有IPC

**概要**: Linux の D-Bus やプラットフォーム固有のIPCを使用。

**利点**: よりネイティブ、他のアプリからの統合が容易
**欠点**: クロスプラットフォーム対応が複雑、Zed既存インフラを活用できない
**工数**: High（5-7日）

### 方法C: ファイルウォッチャーベース

**概要**: 特定のファイルにパスを書き込み、Agentiumがファイル変更を監視。

**利点**: 最もシンプルな実装
**欠点**: レイテンシ、信頼性、応答の受け取り不可
**工数**: Low（1日）

---

## 4. 方法Aの詳細設計

### 4.1 コンポーネント構成

```
crates/agentium/src/main.rs          -- clap引数パーサー追加
crates/agentium/src/agentium.rs      -- add_workspace_with_path() 追加
crates/agentium/src/ipc.rs (new)     -- IPC listener/handler
```

### 4.2 main.rs の変更

```rust
#[derive(Parser)]
struct Args {
    /// プロジェクトパス
    #[arg(short, long)]
    project: Option<PathBuf>,

    /// 環境変数出力（ターミナル作成用）
    #[arg(long, hide = true)]
    printenv: bool,
}

fn main() {
    let args = Args::parse();

    if args.printenv {
        util::shell_env::print_env();
        return;
    }

    // 既存インスタンスが存在するかソケットで確認
    let sock_path = agentium_socket_path();
    if let Some(project_path) = &args.project {
        if try_send_to_running_instance(&sock_path, project_path).is_ok() {
            return; // 既存インスタンスに送信成功
        }
    }

    // 新規起動
    Application::new()
        .with_assets(assets::Assets)
        .run(|cx| {
            // ... 既存の初期化コード ...
            // ソケットリスナー開始
            start_ipc_listener(sock_path, agentium_app_entity, cx);
        });
}
```

### 4.3 AgentiumApp への追加

```rust
impl AgentiumApp {
    fn add_workspace_with_path(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 指定パスの worktree を project に追加
        self.project.update(cx, |project, cx| {
            project.find_or_create_worktree(&path, true, cx)
        }).detach_and_log_err(cx);

        // 新しいワークスペースを追加
        self.add_workspace(window, cx);
    }
}
```

### 4.4 必要な依存クレート追加

```toml
[dependencies]
clap = { workspace = true }
# ipc-channel は cli クレート経由で既に利用可能
```

---

## 5. 考慮事項

### 5.1 プロジェクトの共有 vs 分離

現在のAgentiumは**単一の `Entity<Project>`** を全ワークスペースで共有している（`agentium.rs:25`）。

**選択肢**:
- **共有（現状）**: 全ワークスペースが同じ worktree を参照。Git状態などが共有される。新しいパスは worktree として追加。
- **分離（要変更）**: ワークスペースごとに独立した `Project` を作成。変更が大きいが、独立した作業環境を提供。

**推奨**: まず共有モデルで実装し、必要に応じて分離モデルに移行。

### 5.2 `--printenv` との互換性

`CLAUDE.md` に記載の通り、ターミナル作成時に `--printenv` が必要。clap導入時にこのフラグが引き続き動作することを確認する必要がある。

### 5.3 ウィンドウフォーカス

既存インスタンスにパスを送信した際、ウィンドウをフォアグラウンドに持ってくる必要がある。GPUI の `cx.activate(true)` が利用可能（`main.rs:129`）。

---

## 6. 主要ファイルパス

| 対象 | ファイルパス |
|---|---|
| Agentium エントリーポイント | `crates/agentium/src/main.rs` |
| AgentiumApp 実装 | `crates/agentium/src/agentium.rs` |
| Zed CLI 引数パーサー | `crates/cli/src/main.rs:46-137` |
| CLI IPC 型定義 | `crates/cli/src/cli.rs` |
| Zed IPC ハンドラー | `crates/zed/src/zed/open_listener.rs` |
| Linux ソケットリスナー | `crates/zed/src/zed/open_listener.rs:277-299` |
| Linux CLI launch | `crates/cli/src/main.rs:840-856` |
| workspace::open_paths | `crates/workspace/src/workspace.rs:8621` |
| Project::find_or_create_worktree | `crates/project/src/project.rs` |
