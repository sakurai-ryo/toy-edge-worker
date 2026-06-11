# toy-edge-worker 設計

学習目的で Cloudflare Workers 風の **V8 Isolate ベースの Worker ランタイム**を Rust で構築する。
前作 [toy-lambda-runtime](https://github.com/sakurai-ryo/toy-lambda-runtime)（Firecracker microVM = プロセス分離）に対し、
今回は **Isolate 分離**で 1 プロセス内に多数のテナントを軽量に共存させるモデルを学ぶ。

## 確定した方針

- **rusty_v8（crate `v8`）を直接使う**。deno_core は使わない。
  Isolate / Context / HandleScope / モジュールロード / イベントループを自前で扱い、学習価値を最大化する。
- **Worker API は ES Modules 形式**（現行 Cloudflare Workers 標準）:
  ```js
  export default {
    async fetch(request, env, ctx) {
      return new Response("Hello!");
    },
  };
  ```
- 学習価値が高い部分（V8 glue・イベントループ・Isolate プール・リソース制限）は自前実装し、
  そうでない部分は既存 crate に頼る: HTTP サーバ = hyper、HTTP クライアント = reqwest、TOML = toml。
- 開発環境は macOS arm64。`v8` crate は prebuilt static lib が自動ダウンロードされるためソースビルド不要。

### rusty_v8 の API 世代に関する注意

現行の rusty_v8（本リポジトリは **v8 = "149"** に固定）はスコープ API が
「PinScope + マクロ」方式に全面改訂されている。古い記事の
`HandleScope::new(&mut isolate)` スタイルはコンパイルできない。

```rust
v8::scope!(let scope, &mut isolate);            // HandleScope
let cs = &mut v8::ContextScope::new(scope, ctx); // ContextScope
v8::scope!(let scope, cs);                       // Context 付き PinScope
v8::tc_scope!(let tc, scope);                    // TryCatch
v8::callback_scope!(unsafe scope, context);      // コールバック内での復元
```

- 関数に渡すスコープ型は `&mut v8::PinScope`（TryCatch は Deref 経由でそのまま渡せる）
- スコープを跨いで保持するハンドルは `v8::Global<T>`（`Local::new(scope, &global)` で復元）
- 一次資料: `~/.cargo/registry/src/*/v8-149.*/examples/process.rs` と同 `src/*.rs`

## アーキテクチャ

```
client ──HTTP──> [hyper front (tokio runtime)]
                   │ 1. Host 完全一致 → path 最長プレフィックスでテナント解決
                   │ 2. body 収集 → protocol::WorkerRequest 構築
                   │ 3. hash(tenant) % W で worker スレッド選択
                   │    mpsc::Sender<Msg> へ送信、oneshot で応答待ち
                   ▼
   ┌─ worker thread ×W（素の std::thread、tokio 非参加）─────────────┐
   │  loop {                                                        │
   │    recv Msg::Work { tenant, request, reply }                   │
   │    Pool<Worker>（per-thread LRU）から warm 取得 or cold start    │
   │    Worker::handle:                                             │
   │      isolate.enter() → Request 構築 → fetch ハンドラ呼出        │
   │      → ミニイベントループで Promise 解決待ち                     │
   │      → Response シリアライズ → isolate.exit()                   │
   │    oneshot::Sender で WorkerResponse 返却                       │
   │  }                                                             │
   └────────────────────────────────────────────────────────────────┘
   [watchdog thread]  (IsolateHandle, deadline, 世代) を受信、
                      期限超過で terminate_execution()
   [tokio runtime]    サブリクエスト fetch() の reqwest I/O を実行
```

### スレッディングモデルの設計判断

- **isolate-per-thread 固定**: `OwnedIsolate` は Send だが同時に 1 スレッドからしか
  使えない。テナントを `hash % W` で固定スレッドに割り当て、Isolate は生成スレッド
  から出さない。スレッド間に渡すのは `IsolateHandle`（Send + Sync）だけ。
- **JS 実行は tokio に載せない**: JS 実行はブロッキングなので専用 OS スレッドで行う。
  tokio との接点は (a) mpsc/oneshot チャネル、(b) fetch op が使う
  `tokio::runtime::Handle` のみ。
- **1 スレッド複数 Isolate**: V8 の enter/exit はスレッドローカルなスタックなので、
  Worker は「`new()` の最後に exit して park、`handle()` の間だけ enter」する。
  `OwnedIsolate` の Drop は current であることを要求するため、`Drop for Worker` で
  enter してからフィールド drop（Global 群 → isolate の順）に進む。

## crate 構成

| crate | 責務 |
|---|---|
| `edged`（bin `tew`） | CLI（eval / invoke / serve）、hyper フロント、worker スレッド配線 |
| `runtime` | V8 の全て: Isolate 生成、bootstrap.js、ESM ロード、ops、イベントループ、リソース制限。v8 依存はこの crate に閉じる |
| `pool` | LRU プール（ジェネリクスで v8 非依存）とテナント別メトリクス |
| `router` | workers.toml のパース、Host/Path ルーティング |
| `protocol` | WorkerRequest / WorkerResponse（hyper にも v8 にも依存しない共通型） |

## ランタイムコア（crates/runtime）

### ESM ロード

`ScriptOrigin`（`is_module: true`）→ `script_compiler::compile_module` →
`instantiate_module(scope, resolve_callback)` → `evaluate`。

- 現行 V8 では `evaluate()` は **top-level await の有無に関わらず常に Promise を返す**
  ため、イベントループで解決を待ってから `module.get_status()` を確認する。
- resolve callback はキャプチャ不可・スコープなしで呼ばれる。`v8::callback_scope!`
  で復元する。**import は未対応**（throw する）。対応する場合は deno_core の
  ModuleMap 方式（instantiate 前に依存グラフを再帰 compile して登録、callback は
  マップを引くだけ）を踏襲する。
- 評価後 `get_module_namespace()` から `default.fetch` を `Global<Function>` で保持。

### ミニイベントループ（deno_core の run_event_loop 相当）

`MicrotasksPolicy::Explicit` を設定し、microtask は明示的に流す。

```
loop {
  perform_microtask_checkpoint()        // .then / await 継続をまとめて実行
  if is_execution_terminating  → Terminated
  対象 Promise の state() を確認        // Fulfilled / Rejected なら終了
  if pending_ops == 0          → Hung   // 永遠に解決しない Promise
  op_rx.recv_timeout(wall_deadline)     // op 完了待ち = イベントループの待機点
    → complete_op: resolver を resolve/reject（次周の checkpoint で継続が走る）
}
```

- op の状態（resolver テーブル、pending 数、tokio Handle）は
  `isolate.set_slot(Rc<RefCell<OpState>>)` で保持し、ネイティブ関数から
  `scope.get_slot()` で取り出す。
- `resolver.resolve()` しても Explicit ポリシーでは JS は走らず、次の
  checkpoint でまとめて走る（deno_core と同じモデル）。
- terminate / timeout 時は resolver テーブルを全クリアし、遅れて届いた完了通知は
  「テーブルに無い op_id → 無視」で処理する。

### ops（Rust ⇔ JS）と bootstrap.js

線引き: **I/O・時間・バイト列変換だけが Rust op、Web API の仕様準拠ロジックは全部 JS**。

- Rust ops: `print`（console の実体）、`timer`（setTimeout の実体）、
  `fetch`（サブリクエスト）、`encodeUtf8` / `decodeUtf8`
- bootstrap.js（classic script として Context 作成直後に評価、snapshot なし）:
  Headers / Request / Response / TextEncoder / TextDecoder / console / setTimeout / fetch
- 隠蔽: ops は `globalThis.__ops` 経由で bootstrap の IIFE に渡し、評価後に delete。
  ランタイム内部ヘルパ `__runtime.buildRequest / serializeResponse` も Rust 側が
  `Global<Function>` として取り出してから delete し、ユーザーコードから不可視にする。

### サブリクエスト fetch()（M6）

```
JS: fetch(input, init)
 → bootstrap が Request に正規化し __ops.fetch(url, method, headers, bodyAb) を呼ぶ
 → Rust: PromiseResolver を作って Global 化し op テーブルへ登録、Promise を即 return
 → tokio Handle::spawn で reqwest 実行（worker スレッドはブロックしない）
 → 完了時に mpsc で LoopEvent::OpComplete { op_id, result } を送信
 → イベントループの recv が wake → complete_op が resolver.resolve
 → 次の checkpoint で await の継続が走る
```

`Promise.all` による並行 fetch も pending_ops カウントとテーブルで自然に動く。

### リソース制限（M5）

- **CPU 時間**: watchdog スレッド方式。JS 実行スライス（ハンドラ呼出・microtask
  checkpoint 等）を `CpuMeter::run` で包み、スライス開始時に残予算で arm、終了時に
  disarm、所要時間を予算から差し引く。期限超過で `terminate_execution()`。
  - **世代カウンタ**で「disarm が遅れて次のリクエストを誤爆」を防ぐ。
  - スライスの wall-clock 合計による近似なので、**イベントループが op 完了を待つ
    時間（= await 中）は予算を消費しない**（Cloudflare の「await は CPU 時間に
    入らない」挙動の再現）。厳密にやるなら実行スレッドの
    `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` をサンプリングする（発展課題）。
  - CPU 超過後は `cancel_terminate_execution()` で **Isolate を再利用**する。
- **メモリ**: `CreateParams::heap_limits(0, heap_mb)` + `add_near_heap_limit_callback`。
  コールバックで terminate を要求しつつ **limit を 8MiB 引き上げて返す**
  （引き上げないと例外巻き戻し中の割り当てで V8 が process abort する）。
  原因分類用の AtomicBool フラグを立て、**メモリ超過の Isolate は再利用せず破棄**
  （次のリクエストは cold start）。
- terminate が効くのは JS 実行中のみ。recv ブロック中はイベントループ側の
  wall-clock deadline が効く（二段構え）。

## Isolate プール（M4）

- worker スレッドごとに `Pool<Worker>`（容量 = `isolates_max / worker_threads`）。
- LRU は論理時計 + 線形探索の自前実装（エントリ数が小さいので十分）。
- evict 時は Worker を drop（enter → Global 群 → isolate の順に破棄）。
- メトリクスは `GET /__admin/metrics`（JSON）:
  requests / errors / cold_starts / warm_hits / evictions /
  cold_start_ms (last/max/sum) / terminations_cpu / terminations_mem

## ルーティング（M3）

workers.toml（リポジトリルートにサンプルあり）:

```toml
[server]
listen = "127.0.0.1:8787"
worker_threads = 4

[limits]
cpu_ms = 50          # CPU 予算（JS 実行スライスの合計）
heap_mb = 64         # V8 ヒープ上限
isolates_max = 16    # プロセス全体の warm isolate 上限
wall_timeout_ms = 10000

[[workers]]
name = "hello"
script = "examples/workers/hello.js"
route = { host = "hello.localhost" }   # Host 完全一致
env = { GREETING = "hi" }

[[workers]]
name = "echo"
script = "examples/workers/echo.js"
route = { path = "/echo" }             # path 最長プレフィックス一致
```

解決順: Host 完全一致（ポート除去後）→ path 最長プレフィックス → 404。

## デモ手順（各マイルストーン）

```sh
# M0: V8 hello
cargo run -p edged -- eval "1+2*3"                 # => 7

# M1: 単一 ESM worker の CLI 実行
cargo run -p edged -- invoke examples/workers/hello.js --url /foo --env GREETING=hi

# M2/M3: HTTP サーバ + マルチテナントルーティング
cargo run -p edged -- serve --config workers.toml
curl -H "Host: hello.localhost" http://127.0.0.1:8787/
curl -X POST --data ping http://127.0.0.1:8787/echo/x

# M4: プールとメトリクス（cold/warm、eviction は isolates_max を絞ると観察しやすい）
curl http://127.0.0.1:8787/__admin/metrics

# M5: リソース制限
curl -w "%{http_code}" http://127.0.0.1:8787/loop      # CPU 超過 → 503（isolate 再利用）
curl -w "%{http_code}" http://127.0.0.1:8787/membomb   # ヒープ超過 → 503（isolate 破棄）

# M6: サブリクエスト fetch
curl http://127.0.0.1:8787/proxy
```

単体テスト: `cargo test --workspace`（router のマッチング、pool の LRU/メトリクス）。

## 発展課題（M7 以降）

- **動的デプロイ API**: `PUT /__admin/workers/{name}` でスクリプト差し替え。
  ルートテーブルの ArcSwap 化と、全スレッドの warm isolate invalidate が必要。
- **V8 snapshot による cold start 短縮**: `Isolate::snapshot_creator` で bootstrap
  評価済み Context を blob 化し `CreateParams::snapshot_blob` で復元。Rust ops の
  関数ポインタは `external_references` として作成時と復元時に同一順で登録が必要
  なため、「op に触らない純 JS 部分だけ snapshot し、ops の結線は復元後」の二段構成
  にするのが deno 流。
- **body ストリーミング**: JS 側 ReadableStream シム + pull 型 op（チャンクごとに
  Promise を返す）+ Rust 側 chunk チャネル。OpResult enum と resolver テーブルの
  構造はそのまま使える。
- **厳密な CPU 時間計測**: スレッド CPU 時計のサンプリング方式へ。
- **import 対応**: ModuleMap + 動的 `import()`
  （`set_host_import_module_dynamically_callback`）。
- **graceful shutdown**: `Msg::Shutdown` の配線。

## 参考資料

- rusty_v8 examples（現行スコープ API の一次資料）:
  https://github.com/denoland/rusty_v8/tree/main/examples
- deno_core（イベントループ / ModuleMap / ops の参照実装）:
  https://github.com/denoland/deno_core
- workerd（本家の設計感。isolate-per-worker、リクエスト中ロック）:
  https://github.com/cloudflare/workerd
- Cloudflare blog: "Cloud Computing without Containers"（isolate モデルの背景）
- V8 公式: https://v8.dev/docs/embed / https://v8.dev/features/top-level-await
