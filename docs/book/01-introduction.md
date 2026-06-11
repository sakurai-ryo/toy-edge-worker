# はじめに — 何を作り、何を学ぶのか

## 完成品から見る

本プロジェクトの完成品は、`tew`（toy edge worker）という 1 つのバイナリです。
ES Modules 形式の worker スクリプトを複数デプロイし、HTTP リクエストを
ホスト名やパスで振り分けて実行します。

```sh
$ cargo run -p edged -- serve --config workers.toml
listening on http://127.0.0.1:8787

$ curl -H "Host: hello.localhost" http://127.0.0.1:8787/
{"hello":"world","url":"http://hello.localhost/","greeting":"hi"}

$ curl http://127.0.0.1:8787/proxy        # worker 内から外部 fetch
{"target":"https://example.com/","upstreamStatus":200,...}

$ curl http://127.0.0.1:8787/loop         # 無限ループは 50ms で殺される
worker exceeded CPU time limit            # → HTTP 503

$ curl http://127.0.0.1:8787/membomb      # メモリ爆弾も殺される
worker exceeded memory limit              # → HTTP 503
```

worker スクリプトは現行の Cloudflare Workers と同じ ES Modules 形式です。

```js
// examples/workers/hello.js
export default {
  async fetch(request, env, ctx) {
    console.log(`${request.method} ${request.url}`);
    await new Promise((resolve) => setTimeout(resolve, 10));
    return Response.json({ hello: "world", url: request.url });
  },
};
```

`Request` / `Response` / `Headers` / `fetch` / `console` / `setTimeout` —
これらの Web API はすべて本プロジェクトで自作したものです。
Node.js も Deno も使いません。**V8 という JavaScript エンジンを Rust に
埋め込み、その上のランタイム層をすべて自分で書きます。**

## なぜ作るのか

Cloudflare Workers は「世界中のエッジで、誰のコードでも、ミリ秒で起動して
動かす」というサービスです。これを支えるのが **V8 Isolate** による分離です。

普通のサーバーレス（AWS Lambda など）はテナントごとに microVM やコンテナを
立てます。一方 Cloudflare は **1 つの OS プロセスの中に何千ものテナントを
共存**させます。プロセスを分けないのにテナント同士が干渉しない。この魔法の
種が Isolate です。

この設計には鋭いトレードオフがあります。

- 起動が桁違いに速い（ミリ秒未満）。メモリも桁違いに少ない
- その代わり、分離はプロセス境界より弱い。カーネルではなく
  **V8 のサンドボックスとランタイム設計**が安全性を担う

「ランタイム設計が安全性を担う」── つまり、ランタイムを自作してみれば、
何が難しくて何が本質なのかが分かるはずです。これが本プロジェクトの動機です。

## 何を自作し、何を借りるか

前作 toy-lambda-runtime から引き継いだ方針です:
**学習価値が高い部分は自前実装し、そうでない部分は既存のものに頼る。**

| 自作する（学びの本体） | 借りる（学びの本体ではない） |
|---|---|
| V8 の埋め込み（Isolate/Context/Scope 管理） | V8 そのもの（rusty_v8 のバイナリ） |
| ES Modules のロードと評価 | HTTP サーバ（hyper） |
| イベントループ（microtask + 非同期 op） | HTTP クライアント（reqwest） |
| Web API シム（Request/Response/Headers…） | TOML パーサ（toml） |
| Isolate プール・LRU・メトリクス | 非同期ランタイム（tokio） |
| リソース制限（watchdog・heap limit） | |

特に重要な選択が「**deno_core を使わない**」ことです。deno_core は Deno の
中核ライブラリで、`JsRuntime`・ops・イベントループ・ModuleMap など、本書で
作るものすべてを完成品として提供しています。便利すぎるのです。それを使うと
「V8 の上にランタイムを作る」学びがそっくり抜け落ちます。本書では rusty_v8
（V8 の生バインディング）だけを使い、deno_core 相当の層を自分で書きます。
deno_core は「答え合わせ用の参照実装」として随所で参照します。

## マイルストーン

プロジェクトは「常に動くデモがある」状態を保ちながら、7 段階で進めました。
本書の構成もこれに沿っています。

| M | ゴール | デモ |
|---|---|---|
| M0 | workspace 雛形 + V8 で式を評価 | `tew eval "1+2*3"` → `7` |
| M1 | 単一 ESM worker を CLI で実行 | `tew invoke hello.js --url /foo` |
| M2 | HTTP サーバ化 | `curl localhost:8787/` |
| M3 | マルチテナント + ルーティング | Host/path で別 worker が応答 |
| M4 | Isolate プール・warm 再利用・LRU | metrics で cold/warm/eviction を観測 |
| M5 | リソース制限（CPU・メモリ） | `loop.js` / `membomb.js` → 503 |
| M6 | サブリクエスト fetch() | `proxy.js` が外部 URL を取得 |

## 前作との対比 — 分離技術のスペクトラム

```
強い分離・重い                                          弱い分離・軽い
◀──────────────────────────────────────────────────────────────▶

 物理マシン   VM    microVM      コンテナ    プロセス    V8 Isolate
                   (Firecracker)  (runc)                (Workers)
                      ▲                                    ▲
                      │                                    │
               toy-lambda-runtime                   toy-edge-worker
                  （前作）                              （本作）
```

前作はゲストカーネルごと分離する世界（コールドスタート数百 ms、
メモリ数十 MB〜）でした。本作は同じプロセスのヒープを共有する世界
（コールドスタート 1ms 未満、メモリ数 MB〜）です。
実測は第 12 章で行いますが、本プロジェクトの cold start は **約 0.7ms**、
warm hit ならゼロです。前作の microVM 起動と比べると 2〜3 桁速い。
この差がエッジコンピューティングを成立させています。
