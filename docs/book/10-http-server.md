# HTTP フロントと worker スレッド

M2 です。CLI で 1 回呼べるだけだった Worker を、HTTP サーバとして
公開します。この章の主題はただ 1 つ:
**ブロッキングな JS 実行と、非同期な HTTP サーバを、どう安全に同居させるか。**

## やってはいけない設計から始める

一番素朴な設計はこうです。

```rust
// ❌ アンチパターン
async fn handle(req: Request<Incoming>) -> Response<...> {
    let response = WORKER.lock().await.handle(&worker_request);  // JS 実行
    ...
}
```

`Worker::handle` は worker の Promise が解決するまで**ブロック**します
（内部でイベントループが `recv_timeout` で眠る）。これを tokio のワーカ
スレッドで直接呼ぶと:

- その間、そのスレッドに載っている**他のすべての非同期タスクが進まない**
- worker が `while(true)` したら、tokio ワーカスレッドが 1 本死ぬ。
  テナント数 > コア数なら、悪意ある（または下手な）worker 数個で
  **サーバ全体がハング**する

`tokio::task::spawn_blocking` で逃がす手もありますが、Isolate は
「作ったスレッドに固定したい」（第 11 章）ので、blocking プールに毎回
違うスレッドで載るのは都合が悪い。そこで**専用スレッド + チャネル**です。

## 採用した設計

```
[tokio: hyper front]                       [worker thread (std::thread)]
        │                                            │
        │  mpsc::Sender<Msg> ────────────────────▶  │ rx.recv() でブロック待ち
        │      Msg::Work(WorkItem {                  │
        │        tenant, request,                    │ worker.handle(&request)
        │        reply: oneshot::Sender })           │   （JS 実行・ブロッキング）
        │                                            │
        │  ◀──────────────── oneshot::Sender::send  │ 結果を返す
        │  reply_rx.await                            │
```

```rust
// crates/edged/src/server.rs（抜粋）
pub struct WorkItem {
    pub tenant: String,
    pub request: WorkerRequest,
    pub reply: oneshot::Sender<Result<WorkerResponse, WorkerError>>,
}

pub enum Msg {
    Work(WorkItem),
    Shutdown,          // graceful shutdown 用（未配線）
}
```

### チャネルの選定理由

| 区間 | 使うもの | 理由 |
|---|---|---|
| フロント → worker | `std::sync::mpsc` | 受け手（worker スレッド）は同期世界。`recv()` でブロックして眠りたい |
| worker → フロント | `tokio::sync::oneshot` | 送り手（worker）は同期文脈から `send()` できる（await 不要）。受け手（tokio）は `.await` で待てる |

`tokio::sync::oneshot` が「同期から送って非同期で受ける」橋として機能する
のがポイントです。逆向き（`std::sync::mpsc` を tokio 側から send）も
`send` は非ブロッキングなので成立します。**ブロックする操作（recv）を
どちらの世界でやるか**だけ間違えなければよい。

`Msg` を enum にしてあるのは将来の拡張点（Shutdown、第 15 章の構想だった
op 完了の経路など）を見越した形です。リクエストが詰まるチャネルの形を
あとから変えるのは大工事なので、最初に enum にしておきます。

## worker スレッド本体

```rust
fn worker_thread(/* ... */) {
    // Worker は遅延生成（cold start は第 12 章でプール化）
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Work(item) => {
                let result = worker.handle(&item.request);
                // 受信側が先に消えていても無視してよい
                let _ = item.reply.send(result);
            }
            Msg::Shutdown => break,
        }
    }
}
```

`item.reply.send()` の結果を捨てているのは意図的です。クライアントが
curl を Ctrl-C した場合、フロント側の `reply_rx` は drop されており
send は失敗しますが、それは worker スレッドにとってどうでもよい
（処理は終わったし、誰も結果を待っていない）。
**「返事の宛先が消えていることは正常系」**は、リクエスト/レスポンス型の
スレッド間通信の定番です。

## hyper 側 — リクエストの収集と返却

hyper 1.x はかなり低レベルになったので、グルーコードを少し書きます。

```rust
async fn handle_http(req: Request<Incoming>, /* ... */)
    -> Result<Response<Full<Bytes>>, hyper::Error>
{
    // hyper Request → protocol::WorkerRequest
    let request = collect_request(req, &host).await?;   // body を全部読む

    let (reply_tx, reply_rx) = oneshot::channel();
    sender.send(Msg::Work(WorkItem { tenant, request, reply: reply_tx }))?;

    match reply_rx.await {
        Ok(Ok(response)) => { /* WorkerResponse → hyper Response */ }
        Ok(Err(err)) => {
            let status = match err {
                WorkerError::Timeout | WorkerError::Terminated
                | WorkerError::CpuExceeded | WorkerError::MemExceeded
                    => StatusCode::SERVICE_UNAVAILABLE,   // 503: 制限・過負荷系
                _ => StatusCode::INTERNAL_SERVER_ERROR,   // 500: スクリプト・内部
            };
            Ok(plain_response(status, &err.to_string()))
        }
        Err(_) => /* worker がパニックして reply を drop した場合など */
    }
}
```

`collect_request` では URL を再構成します。worker に渡す `request.url` は
絶対 URL（Cloudflare 互換）なので、`Host` ヘッダとパスから
`http://{host}{path}` を組みます。body は `BodyExt::collect()` で
全読みします（ストリーミング非対応の割り切り）。

## ここでの並行度の整理

この時点（M2）の構成は worker スレッド 1 本です。並行度は:

- HTTP の受け付け・パース・書き出し: tokio 任せで高並行
- JS 実行: **直列**（1 スレッドに 1 リクエストずつ）

「JS が直列」は一見ひどい制約に見えますが、(a) スレッド数 W は設定で
増やせる（第 11 章）、(b) 1 リクエストの JS 実行は本来短い（長い I/O は
await で待つだけ → 第 15 章の fetch 中も worker スレッドはブロックして
いる、がそれは次章以降の改善余地）、という前提なら成立します。
本家 workerd も「1 つの Isolate は同時に 1 リクエスト処理中はロック」
というモデルです。

## 動かす

```sh
$ cargo run -p edged -- serve --script examples/workers/hello.js &
listening on http://127.0.0.1:8787

$ curl -i http://127.0.0.1:8787/foo
HTTP/1.1 200 OK
content-type: application/json
...
{"hello":"world","url":"http://127.0.0.1:8787/foo","greeting":null}
```

## この章のまとめ

- JS 実行（ブロッキング）は専用 std::thread、HTTP は tokio。
  **2 つの世界の接点はチャネルだけ**
- `std::mpsc`（同期側が recv でブロック）+ `tokio oneshot`
  （同期から send、非同期で await）が橋の定番
- 返事の宛先が消えているのは正常系。send の失敗は握りつぶす
- エラーは「クライアントに見せる HTTP ステータス」へここで写像する
  （500 = スクリプトの責任、503 = 制限・過負荷）
