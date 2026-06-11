# 付録 C: コマンド早見表

## ビルド・テスト

```sh
cargo build -p edged          # tew バイナリ（初回は v8 の DL が走る）
cargo test --workspace        # router / pool のユニットテスト
```

## CLI（tew）

```sh
# M0: 式の評価
cargo run -p edged -- eval "1+2*3"

# M1: worker を 1 回呼ぶ
cargo run -p edged -- invoke examples/workers/hello.js \
    --url /foo \
    --method POST \
    --body 'hello' \
    --header 'Content-Type: text/plain' \
    --env GREETING=hi

# M2〜: HTTP サーバ
cargo run -p edged -- serve --config workers.toml
cargo run -p edged -- serve --script examples/workers/hello.js   # 単一スクリプトモード
cargo run -p edged -- serve --config workers.toml --listen 127.0.0.1:9999
```

## デモ用 curl 集

```sh
# Host ルーティング（M3）
curl -H "Host: hello.localhost" http://127.0.0.1:8787/

# path ルーティング（M3）
curl -X POST --data ping http://127.0.0.1:8787/echo/x

# 404
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8787/nothing

# メトリクス（M4）
curl -s http://127.0.0.1:8787/__admin/metrics | jq .

# CPU 制限（M5）: ~100ms で 503 が返る
time curl -s -w "%{http_code}\n" http://127.0.0.1:8787/loop

# メモリ制限（M5）: 503 + isolate 破棄（cold_starts が毎回増える）
curl -s -w "%{http_code}\n" http://127.0.0.1:8787/membomb

# サブリクエスト fetch（M6）
curl -s http://127.0.0.1:8787/proxy | jq .
```

## eviction を観察する（M4）

worker_threads = 1、isolates_max = 2 の設定で 3 テナントを順に叩くと
LRU eviction が起きる。

```sh
curl -s http://127.0.0.1:8787/hello  > /dev/null   # cold hello
curl -s http://127.0.0.1:8787/echo/x > /dev/null   # cold echo
curl -s http://127.0.0.1:8787/hello  > /dev/null   # warm hello（echo が最古に）
curl -s http://127.0.0.1:8787/third  > /dev/null   # cold third → echo evict
curl -s http://127.0.0.1:8787/__admin/metrics | jq 'map_values({cold_starts, warm_hits, evictions})'
```

## トラブル時

```sh
pkill -f "target/debug/tew"        # 残存サーバの掃除
lsof -nP -iTCP:8787 -sTCP:LISTEN   # ポートの主を確認
```

## 本書のビルド

```sh
cd docs
mdbook serve    # http://localhost:3000 でプレビュー
mdbook build    # docs/out/ に静的サイト生成
```

## workers.toml の最小形

```toml
[server]
listen = "127.0.0.1:8787"
worker_threads = 4

[limits]
cpu_ms = 50
heap_mb = 64
isolates_max = 16
wall_timeout_ms = 10000

[[workers]]
name = "hello"
script = "examples/workers/hello.js"
route = { host = "hello.localhost" }   # または route = { path = "/hello" }
env = { GREETING = "hi" }
```
