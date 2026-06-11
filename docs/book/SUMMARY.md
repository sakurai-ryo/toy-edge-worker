# Summary

[本書について](README.md)

# 第 I 部　全体像をつかむ

- [はじめに — 何を作り、何を学ぶのか](01-introduction.md)
- [Cloudflare Workers と Isolate モデル](02-isolate-model.md)
- [全体設計と技術選定](03-architecture.md)

# 第 II 部　V8 を理解する

- [V8 埋め込みの基礎概念](04-v8-concepts.md)
- [rusty_v8 の現行 API と M0: eval](05-rusty-v8.md)

# 第 III 部　Worker ランタイムのコア

- [ES Modules をロードする](06-esm-loading.md)
- [Web API シムと ops 設計](07-bootstrap-and-ops.md)
- [イベントループを自作する](08-event-loop.md)
- [リクエスト 1 件の旅](09-request-lifecycle.md)

# 第 IV 部　HTTP サーバとマルチテナント

- [HTTP フロントと worker スレッド](10-http-server.md)
- [マルチテナントとルーティング](11-multi-tenant.md)
- [Isolate プールとコールドスタート](12-isolate-pool.md)

# 第 V 部　信頼できないコードを安全に動かす

- [CPU 時間制限 — watchdog と terminate](13-cpu-limit.md)
- [メモリ制限 — heap limit との攻防](14-memory-limit.md)

# 第 VI 部　非同期 I/O

- [サブリクエスト fetch() — Promise と tokio をつなぐ](15-subrequest-fetch.md)

# 第 VII 部　まとめと発展

- [まとめと発展課題](16-conclusion.md)

---

[付録 A: rusty_v8 API 早見表](appendix-a-v8-api.md)
[付録 B: ハマりどころ集](appendix-b-troubleshooting.md)
[付録 C: コマンド早見表](appendix-c-commands.md)
