# サブリクエスト fetch() — Promise と tokio をつなぐ

最後のマイルストーン M6 です。worker の中から外の世界へ HTTP リクエストを
出せるようにします。

```js
// examples/workers/proxy.js
export default {
  async fetch(request, env) {
    const upstream = await fetch(env.TARGET ?? "https://example.com/");
    const text = await upstream.text();
    return Response.json({ upstreamStatus: upstream.status, bytes: text.length });
  },
};
```

技術的なテーマは「**JS の Promise と Rust の async/await という、
2 つの非同期世界を 1 本につなぐ**」ことです。第 8 章で作った op の
仕組みがそのまま器になります。

## 全体の流れ

```
JS (worker スレッド)                 tokio ランタイム
────────────────────                ─────────────────
await fetch(url)
  └ __ops.fetch(url, method,
                headers, body)
       │ op_fetch (Rust)
       │ 1. PromiseResolver 作成
       │ 2. resolvers[op_id] に登録
       │ 3. handle.spawn(async move {  ──▶  reqwest::send().await
       │      ...                            body 収集
       │    })                               │
       │ 4. Promise を即 return              │
       ▼                                     │
イベントループ:                               │
  op_rx.recv() でブロック  ◀──── tx.send(OpComplete { op_id, result })
  complete_op:
    resolvers から取り出し
    JS オブジェクトを組んで resolve（予約）
  perform_microtask_checkpoint()
    → await の続きが走る
```

worker スレッドは reqwest の I/O を**一切待ちません**。spawn したら
すぐ Promise を返してイベントループに戻り、`recv` で眠ります。
I/O は tokio のスレッドプールが進め、完了だけがチャネルで届きます。

## tokio::runtime::Handle — 同期世界から非同期世界への送り口

worker スレッドは tokio に参加していない素の OS スレッドです。
そこから tokio に仕事を頼むための道具が `tokio::runtime::Handle` です。

- `Handle` はランタイムへの参照で、clone も Send も自由
- **`handle.spawn(future)` はランタイム外のスレッドからも呼べる**。
  これが同期 → 非同期の正規の入口
- 逆方向（結果を返す）は普通のチャネルでよい（`std::mpsc::Sender::send`
  は非ブロッキングなので async タスクからも呼べる）

Handle はどこから来るか。サーバモードでは hyper を動かしている
ランタイムから取り、worker スレッド生成時に渡します。

```rust
// crates/edged/src/server.rs（抜粋）
let tokio_handle = tokio::runtime::Handle::current();   // serve() は tokio 上で動いている
// → worker_thread(…, tokio_handle) → WorkerConfig { tokio: Some(handle), … }
```

CLI（`tew invoke`）モードでは専用ランタイムを 1 つ作って渡します。
`WorkerConfig::tokio` を `Option` にしておき、None なら worker 内の
`fetch()` は即 reject（"fetch is not available"）という丁寧な縮退に
してあります。

`OpState`（第 8 章）には Handle と reqwest Client を追加しました。

```rust
pub struct OpState {
    // ...（第 8 章のフィールド）...
    tokio: Option<tokio::runtime::Handle>,
    http: reqwest::Client,     // コネクションプール共有のため isolate に 1 つ
}
```

`reqwest::Client` は内部にコネクションプールを持つので、リクエストごとに
作らず使い回します（生成は tokio コンテキスト外でも可能）。

## op_fetch — 4 つの仕事

```rust
// crates/runtime/src/ops.rs（抜粋）
fn op_fetch(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue) {
    // 0. 引数（平坦な値）を Rust 値へ
    let url = args.get(0).to_string(scope)...;
    let method = args.get(1)...;
    let headers = js_header_pairs(scope, args.get(2));
    let body = value_to_bytes(args.get(3));

    // 1. Promise を作って即 return
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    rv.set(resolver.get_promise(scope).into());

    // 2. resolver を Global 化して op テーブルへ
    let state = get_op_state(scope);
    let (op_id, tx, handle, client) = {
        let mut st = state.borrow_mut();
        let Some(handle) = st.tokio.clone() else {
            /* tokio が無い環境: その場で reject して return */
        };
        let resolver = v8::Global::new(scope, resolver);
        (st.register(resolver), st.tx.clone(), handle, st.http.clone())
    };

    // 3. I/O を tokio へ
    handle.spawn(async move {
        let result = do_fetch(client, url, method, headers, body).await;
        // worker スレッドは op_rx.recv でブロック中 → この send が wake になる
        let _ = tx.send(LoopEvent::OpComplete { op_id, result });
    });
}
```

`do_fetch` は素直な reqwest 呼び出しです（メソッドのパース、ヘッダ転写、
body 添付、ステータス・ヘッダ・body の回収）。エラーは `String` に潰して
`Err` で返し、complete_op 側で JS の `TypeError` になります —
本物の fetch がネットワークエラーを TypeError で reject するのと
同じ意味論です。

### 完了側 — JS オブジェクトに組み立てて resolve

第 8 章の `complete_op` に Fetch アームを足します。

```rust
Ok(OpResult::Fetch { status, headers, body }) => {
    // { status, headers: [[k,v],...], body: ArrayBuffer } を作って resolve
    let obj = v8::Object::new(scope);
    obj.set(scope, "status", v8::Integer::new_from_unsigned(scope, status as u32));
    obj.set(scope, "headers", headers_to_js(scope, &headers));
    obj.set(scope, "body", ops::bytes_to_array_buffer(scope, body));  // ゼロコピー
    resolver.resolve(scope, obj.into());
}
```

JS 側（bootstrap.js）はこの平坦なオブジェクトから `Response` を組みます。

```js
const fetch = async (input, init) => {
  const req = new Request(input, init);          // 引数正規化は Request に任せる
  const bytes = req._bodyBytes();
  const bodyAb = bytes ? bytes.buffer.slice(...) : undefined;
  const r = await opFetch(req.url, req.method, [...req.headers], bodyAb);
  return new Response(r.body.byteLength > 0 ? r.body : null, {
    status: r.status,
    headers: r.headers,
  });
};
```

第 7 章の方針「境界は平坦な値だけ、オブジェクトの組み立ては JS で」が
ここでも貫かれています。Rust 側は `Request` クラスの存在すら知りません。

## Promise.all が「タダで」動く

並行 fetch を試します。

```js
const [a, b] = await Promise.all([
  fetch("https://example.com/"),
  fetch("https://example.org/"),
]);
```

```sh
$ cargo run -p edged -- invoke /tmp/parallel_fetch.js
{"a":200,"b":200}
```

このために追加したコードは **1 行もありません**。仕組みを追うと:

1. microtask checkpoint 中に `fetch` が 2 回呼ばれ、op が 2 つ
   spawn される（`pending_ops = 2`）。2 つの reqwest は tokio 上で
   **同時に**進行する
2. イベントループは `recv` で眠る。先に終わったほうの OpComplete で起き、
   resolver を resolve、checkpoint。`Promise.all` はまだ Pending
3. もう片方の OpComplete で同様に。`Promise.all` が Fulfilled になり、
   ループ終了

第 8 章で「op テーブル + pending カウンタ + チャネル」という汎用の器を
作ったので、op が何個飛んでいても、どんな順で完了しても、何も特別扱いが
要らないのです。**イベントループの設計が正しいことの何よりの証拠**が、
この「何も書かずに動く Promise.all」です。

## CPU 会計との合流 — await はタダ、を確認する

proxy worker の設定を思い出してください。`cpu_ms = 50` です。
外部 fetch には 200ms 以上かかります。

```sh
$ time curl -s http://127.0.0.1:8787/proxy
{"target":"https://example.com/","upstreamStatus":200,...}
real    0m0.281s     # wall-clock 281ms。CPU 予算 50ms でも完走
```

第 13 章の CpuMeter は「JS 実行スライス」だけを arm します。
`await fetch` の間、worker スレッドは `recv_timeout` で眠っており、
スライスの外 = watchdog は解除されています。**ネットワーク待ちが
CPU 予算を 1ms も消費していない**ことが、この 1 本の curl で確認できます。

wall-clock 側の保険（`wall_timeout_ms = 10000`）は生きているので、
外部サーバが固まっても 10 秒で `Timeout` になります。二段構えです。

## 動かす（まとめ）

```sh
# サーバ経由
$ curl -s http://127.0.0.1:8787/proxy
{"target":"https://example.com/","upstreamStatus":200,"contentType":"text/html",
 "bytes":559,"head":"<!doctype html>..."}

# CLI でも同じ worker が動く（専用 tokio ランタイムを内部で起動）
$ cargo run -p edged -- invoke examples/workers/proxy.js --env TARGET=https://example.com/
```

## この章のまとめ

- 同期世界 → tokio は `Handle::spawn`、tokio → 同期世界は普通の
  channel send。これだけで 2 つの非同期世界がつながる
- op_fetch = 「resolver を預かる → I/O を spawn → Promise を即返す」。
  完了は第 8 章の経路にそのまま乗る
- 境界は平坦な値だけ。Response の組み立ては JS（bootstrap）に任せる
- `Promise.all` は追加コードゼロで動く — 汎用の op 基盤を先に作った配当
- await 中に CPU 予算が減らないことを実測で確認

これで M0〜M6 がすべて完成しました。最終章で全体を振り返ります。
