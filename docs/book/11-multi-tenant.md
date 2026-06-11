# マルチテナントとルーティング

M3 です。いよいよ Isolate モデルの核心 — **1 プロセスに複数のテナントを
共存させる** — に踏み込みます。この章には本プロジェクトで最も
デリケートな unsafe コード（Isolate の enter/exit）が登場します。

## workers.toml — テナントの宣言

```toml
[server]
listen = "127.0.0.1:8787"
worker_threads = 4

[limits]                  # 第 13・14 章で使う
cpu_ms = 50
heap_mb = 64
isolates_max = 16
wall_timeout_ms = 10000

[[workers]]
name = "hello"
script = "examples/workers/hello.js"
route = { host = "hello.localhost" }     # Host 完全一致
env = { GREETING = "hi" }

[[workers]]
name = "echo"
script = "examples/workers/echo.js"
route = { path = "/echo" }               # パス前方一致
```

`router` crate が serde でこれを読み、`RouteTable` を組みます。

## ルーティング — Host 完全一致 → path 最長プレフィックス

```rust
// crates/router/src/lib.rs（抜粋）
pub struct RouteTable {
    host_routes: HashMap<String, String>,
    /// (prefix, tenant)。最長一致のため prefix 長の降順でソート済み
    path_routes: Vec<(String, String)>,
}

impl RouteTable {
    pub fn resolve(&self, host: &str, path: &str) -> Option<&str> {
        let host = host.split(':').next().unwrap_or(host);   // ポート除去
        if let Some(tenant) = self.host_routes.get(host) {
            return Some(tenant);
        }
        self.path_routes
            .iter()
            .find(|(prefix, _)| path.starts_with(prefix.as_str()))
            .map(|(_, tenant)| tenant.as_str())
    }
}
```

決め事は 3 つだけです。

1. `Host` ヘッダの完全一致（`:8787` などのポートは無視）が最優先
2. 次にパスの**最長**プレフィックス一致。`path_routes` を長さ降順に
   ソートしておけば、最初に `starts_with` した要素が最長一致
3. どちらにも当たらなければ `None` → HTTP 404

最長一致のソート済み線形探索は O(ルート数) ですが、テナント数十個の
toy には十分です（本物は trie などにする）。この crate は v8 と無関係な
純ロジックなので、ユニットテストもまっすぐ書けます。

```rust
#[test]
fn path_longest_prefix() {
    let table = RouteTable::from_workers(&[
        def("api", None, Some("/api/")),
        def("api-v2", None, Some("/api/v2/")),
        def("root", None, Some("/")),
    ]);
    assert_eq!(table.resolve("localhost", "/api/v2/users"), Some("api-v2"));
    assert_eq!(table.resolve("localhost", "/api/users"), Some("api"));
    assert_eq!(table.resolve("localhost", "/top"), Some("root"));
}
```

ローカルでの Host ルーティングの検証は `curl -H "Host: ..."` で行います。
DNS をいじる必要はありません。

## テナント → スレッドの固定

worker スレッドを W 本に増やし、テナントをハッシュで固定割り当てします。

```rust
// crates/edged/src/server.rs（抜粋）
let sender = {
    let mut hasher = DefaultHasher::new();
    tenant.hash(&mut hasher);
    &senders[(hasher.finish() % senders.len() as u64) as usize]
};
```

「ラウンドロビンで空いているスレッドへ」ではなく**固定**するのが重要です。
理由は次節の Isolate のスレッド規律そのものです:

- 同一テナントの Isolate は常に同じスレッドにある
  → Isolate がスレッドを跨ぐ事態が構造的に起きない
- 同一テナントのリクエストは直列化される
  → 1 つの Isolate を同時に 2 リクエストが触る事態も構造的に起きない

並行性のバグを「ロックで防ぐ」のではなく「**起き得ない構造にする**」
アプローチです。代償はテナント間の負荷の偏り（ハッシュ次第）ですが、
toy では許容します。

## 1 スレッドに複数の Isolate — enter / exit の規律

ここが本章の核心です。1 つの worker スレッドは複数テナントを担当する
ので、**複数の Isolate を 1 スレッドで持ち回す**必要があります。

### V8 の規律

V8 には「カレント Isolate」という概念があり、スレッドローカルな
**スタック**で管理されています。

- `Isolate::Enter()`: この Isolate をカレントにする（スタックに push）
- `Isolate::Exit()`: カレントから外す（pop。**LIFO 順でなければならない**）
- スコープを作る・JS を実行するなどの操作は、カレントな Isolate に
  対してしか行えない

rusty_v8 では `Isolate::new()` が自動的に enter し、`OwnedIsolate` の
drop が exit + 破棄を行います。**Isolate を 1 個しか使わないなら**
何も意識する必要がない、よくできた設計です。

しかし複数 Isolate を持ち回す場合、この自動 enter が罠になります。

```
Isolate A を new   → A がカレント
Isolate B を new   → B がカレント（スタック: A, B）
A を使いたい…     → ❌ B がカレントのまま A のスコープを作ると未定義動作
B より先に A を drop → ❌ LIFO 違反
```

### 解: park 方式

採用した規律は単純です。
**「使うときだけ enter し、使い終わったら必ず exit する（park する）」**

```rust
// Worker::new の最後
unsafe { isolate.exit() };       // park: カレントから外して保管

// Worker::handle
pub fn handle(&mut self, req: &WorkerRequest) -> Result<WorkerResponse, WorkerError> {
    // SAFETY: Worker は生成スレッドからのみ使われ、enter と exit が対になる。
    unsafe { self.isolate.enter() };
    let result = self.handle_inner(req);
    unsafe { self.isolate.exit() };
    result
}
```

これでスレッドの状態は「リクエスト処理中だけ、その Worker の Isolate が
カレント。それ以外の時間は何もカレントでない」となり、何個の Worker を
持っていても衝突しません。

`enter`/`exit` が `unsafe fn` なのは、「カレントでない Isolate のスコープを
作らない」「enter/exit を LIFO で対にする」という規約をコンパイラが検証
できないからです。unsafe を `handle()` と `new()` の末尾、そして次の
`Drop` の 3 箇所だけに閉じ込め、**`Worker` の公開 API を使う限り規約が
破れない**形にしています。

### Drop にも一手間いる

`OwnedIsolate` の drop は「自分がカレントであること」を assert します。
park 中（exit 済み）の Worker をそのまま drop すると assert に倒れるので、
`Drop` 実装で enter してからフィールドの drop に進みます。

```rust
impl Drop for Worker {
    fn drop(&mut self) {
        // OwnedIsolate の Drop は自分が current であることを要求する。
        // park 状態（exit 済み）なので enter してから drop に進む。
        // Global 群はフィールド宣言順により isolate より先に破棄される。
        unsafe { self.isolate.enter() };
    }
}
```

`Drop::drop` の実行後に各フィールドが宣言順で drop される、という Rust の
仕様（第 9 章の宣言順の話）とここで合流します。enter → Global 群 drop →
OwnedIsolate drop（カレントなので OK）、という一連の流れになります。

## worker スレッドの形（M3 版）

```rust
fn worker_thread(index: usize, rx: mpsc::Receiver<Msg>,
                 tenants: Arc<HashMap<String, TenantSpec>>) {
    let mut workers: HashMap<String, Worker> = HashMap::new();  // 第 12 章で Pool に

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Work(item) => {
                let worker = match get_or_create(&mut workers, &tenants, &item.tenant) {
                    Ok(worker) => worker,            // warm: 既存を再利用
                    Err(err) => { let _ = item.reply.send(Err(err)); continue; }
                };
                let result = worker.handle(&item.request);
                let _ = item.reply.send(result);
            }
            Msg::Shutdown => break,
        }
    }
}
```

Worker は**最初のリクエストが来たときに遅延生成**します（cold start）。
2 回目以降は HashMap から取り出すだけ（warm）。この素朴なキャッシュを
次章で「容量制限つきの LRU プール」に進化させます。

## 動かす

```sh
$ cargo run -p edged -- serve --config workers.toml &

$ curl -H "Host: hello.localhost" http://127.0.0.1:8787/
[worker-1] cold start tenant=hello (4ms)
{"hello":"world","url":"http://hello.localhost/","greeting":"hi"}

$ curl -X POST --data ping http://127.0.0.1:8787/echo/x
[worker-1] cold start tenant=echo (0ms)
{"name":"echo","method":"POST",...,"body":"ping"}

$ curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8787/nothing
404
```

実行ログに注目してください。hello と echo が**同じ worker-1 スレッド**に
割り当てられています（ハッシュの偶然）。つまりこの時点で
「1 スレッド複数 Isolate の enter/exit 持ち回し」が実地で動いています。
2 回目の curl では cold start ログが出ません（warm hit）。

## この章のまとめ

- ルーティングは Host 完全一致 → path 最長プレフィックス → 404
- テナントはハッシュでスレッドに**固定**。並行バグを「起き得ない構造」で殺す
- 複数 Isolate の持ち回しは park 方式（使うときだけ enter）。
  unsafe は 3 箇所に閉じ込め、公開 API からは破れないようにする
- `Drop` では enter してから drop に進む（assert 対策 + Global の drop 順）
