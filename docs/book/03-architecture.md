# 全体設計と技術選定

## 完成形の全体像

最初に完成形のアーキテクチャを示します。以降のすべての章は、この図の
どこかのパーツを作る話です。

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
   │      isolate.enter() → JS Request 構築 → fetch ハンドラ呼出     │
   │      → ミニイベントループで Promise 解決待ち                     │
   │      → Response シリアライズ → isolate.exit()                   │
   │    oneshot::Sender で WorkerResponse 返却                       │
   │  }                                                             │
   └────────────────────────────────────────────────────────────────┘
   [watchdog thread]  (IsolateHandle, deadline, 世代) を受信、
                      期限超過で terminate_execution()
   [tokio runtime]    サブリクエスト fetch() の reqwest I/O を実行
```

登場するスレッドは 3 種類です。

| スレッド | 数 | 役割 |
|---|---|---|
| tokio ワーカ | コア数 | HTTP の受け付け・返却、サブリクエストの I/O。**JS は絶対に実行しない** |
| worker スレッド | W（設定値） | V8 Isolate を所有し JS を実行する。**tokio に参加しない** |
| watchdog | 1 | CPU 時間制限の番人（第 13 章） |

## 最重要の設計判断: JS とイベント駆動の世界を分ける

この設計の核は「**ブロッキングな JS 実行を、tokio の非同期世界から
物理的に分離する**」ことです。

JS の実行はブロッキング操作です。`worker.handle(&request)` は、worker の
Promise が解決するまで返ってきません（その間、内部のイベントループが
microtask を回したり op の完了を待ったりします）。これを tokio のワーカ
スレッド上で実行すると、そのスレッドが占有されて HTTP サーバ全体が
詰まります。最悪、無限ループする worker が 1 つあるだけでプロセス全体が
ハングします。

そこで:

- JS は専用の OS スレッド（`std::thread`）でだけ実行する
- tokio と worker スレッドの接点は **チャネルだけ**にする
  - フロント → worker: `std::sync::mpsc::Sender<Msg>`
  - worker → フロント: `tokio::sync::oneshot::Sender<Result<WorkerResponse, WorkerError>>`
- worker スレッドから tokio に仕事を頼むのは
  `tokio::runtime::Handle::spawn` だけ（第 15 章の fetch）

`tokio::sync::oneshot` は送信側が同期文脈から `send` できる
（`.await` 不要）ため、この橋渡しにちょうど良い部品です。

### Isolate はスレッドに固定する

V8 の Isolate には「同時に 1 スレッドからしか触れない」という鉄則が
あります（詳細は第 4 章）。rusty_v8 の `OwnedIsolate` は `Send`（スレッド間
で移動はできる）ですが、雑に移動させるとライフタイムや enter/exit の管理が
一気に複雑になります。

本プロジェクトの解は単純です:
**テナントを `hash(tenant) % W` で worker スレッドに固定し、Isolate は
生成したスレッドから一生出さない。** スレッド間を渡るのは
`v8::IsolateHandle`（`Send + Sync` な制御用ハンドル。terminate などが
できる）だけです。

副産物として「同一テナントは常に同一スレッド = 同時実行は 1 リクエスト」
というモデルになります。これは本家 workerd が 1 リクエスト処理中に
Isolate のロックを取る挙動の、素朴な近似でもあります。

## cargo workspace 構成

```
toy-edge-worker/
├── Cargo.toml              # workspace（v8 = "149" に固定）
├── workers.toml            # テナント設定サンプル
├── crates/
│   ├── edged/              # [bin: tew] CLI + hyper フロント + スレッド配線
│   ├── runtime/            # [lib] V8 のすべて（このプロジェクトの心臓部）
│   │   └── js/bootstrap.js #   Web API シム（include_str! で埋め込み）
│   ├── pool/               # [lib] LRU プール + メトリクス（v8 非依存）
│   ├── router/             # [lib] workers.toml + Host/Path ルーティング
│   └── protocol/           # [lib] WorkerRequest/Response（hyper にも v8 にも非依存）
└── examples/workers/       # hello.js, echo.js, loop.js, membomb.js, proxy.js
```

依存方向は一方向です:

```
edged ──> runtime ──> protocol
  │  └──> router          ▲
  └─────> pool            │
            └─────────────┘ (pool はジェネリクスで Worker を受けるため
                             実は protocol にも v8 にも依存しない)
```

設計上のポイント:

- **v8 依存は `runtime` だけに閉じる。** v8 crate はビルドが重い
  （静的ライブラリのリンク）ので、依存を 1 crate に隔離すると他の crate の
  イテレーションが軽くなる。型の漏洩も防げる
- **`protocol` は最下層。** `WorkerRequest` / `WorkerResponse` は
  method / url / headers / body(`Vec<u8>`) だけの素朴な構造体で、
  hyper の型にも v8 の型にも依存しない。フロントとランタイムが
  お互いの実装詳細を知らずに会話するための「土管の形」
- **`pool` はジェネリクス**（`Pool<W>`）にして v8 非依存を保つ。
  単体テストが `Pool<u32>` で書ける

## 技術選定の理由

### rusty_v8 を生で使う（deno_core を使わない）

第 1 章で述べたとおり、deno_core を使うと学びの本体（イベントループ、
ops、モジュールロード）が既製品になってしまうため、rusty_v8 を直接使います。

rusty_v8 は Deno チームがメンテナンスする V8 の Rust バインディングで、
次の点で「生の V8 C++ API」とほぼ 1:1 です:

- API の概念（Isolate, HandleScope, Local, ...）がそのまま
- ただし V8 の「スコープはスタック上に積む」という規約を、Rust の
  借用検査とピン留め（`Pin`）で安全に表現している（第 5 章）

なお **バージョン固定（`v8 = "149"`）は重要**です。rusty_v8 は Chrome の
リリースに追随して 4 週ごとにメジャーバージョンが上がり、スコープ API の
形すら変わります（古い記事のコードはまず動きません）。本書のコードは
149 系で検証しています。

### C++ で書かない理由

本家 workerd は C++ です。しかし生 V8 を C++ から使うには depot_tools /
GN という Google 製ビルドシステムで V8 自体をビルドする必要があり、
これだけで数時間〜半日が溶けます。rusty_v8 は **ビルド済み静的ライブラリを
GitHub Releases から自動ダウンロード**するため、`cargo build` 一発で
始められます（macOS arm64 のネイティブ対応もある）。toy プロジェクトの
立ち上がり速度を優先しました。

### hyper / tokio / reqwest

HTTP のパースや非同期 I/O 自体は本プロジェクトの学びではないので、
デファクトに任せます。前作 toy-lambda-runtime と同じ選定です。

## データの流れ（1 リクエスト）

第 9 章までの内容を 1 枚にまとめた、リクエスト 1 件の旅です。

```
1. hyper が HTTP/1.1 をパース
2. router: Host ヘッダ → path で tenant 解決（なければ 404）
3. body を全部読んで WorkerRequest { method, url, headers, body } に詰める
4. hash(tenant) % W 番の worker スレッドへ mpsc 送信、oneshot を await
                  ── ここから worker スレッド ──
5. Pool から warm Worker を取得（なければ cold start: Isolate 生成
   → bootstrap.js 評価 → ESM ロード → fetch ハンドラ取り出し）
6. isolate.enter()
7. WorkerRequest → JS の Request オブジェクト（bootstrap のヘルパで構築）
8. fetch(request, env, ctx) を呼ぶ → 戻り値は Promise
9. ミニイベントループ: microtask を回し、非同期 op（timer/fetch）の完了を
   チャネルで待ち、Promise が settle するまで繰り返す
10. 返ってきた Response を [status, headers, body] にシリアライズ
    （body の読み出しも async なので再びイベントループ）
11. isolate.exit()、oneshot で WorkerResponse を返す
                  ── ここから tokio ──
12. hyper のレスポンスに変換して返却
```

この旅の中に、本書のすべての主題が含まれています。5 が第 6・12 章、
7〜8 が第 7・9 章、9 が第 8 章、そして旅全体を護衛する watchdog が
第 13・14 章です。
