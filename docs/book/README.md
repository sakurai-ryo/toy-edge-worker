# 本書について

本書は、学習用プロジェクト **toy-edge-worker** の解説書です。
Cloudflare Workers のような **V8 Isolate ベースの Worker ランタイム**を、
Rust と rusty_v8（crate `v8`）だけで一から作ります。

```js
// これが動くランタイムを、V8 を生で叩いて作る
export default {
  async fetch(request, env, ctx) {
    const upstream = await fetch("https://example.com/");
    return Response.json({ status: upstream.status });
  },
};
```

## 本書で学べること

- **V8 埋め込みの実際**: Isolate / Context / HandleScope / Local / Global という
  V8 の基本部品を、Rust の所有権と突き合わせながら理解する
- **JS ランタイムの自作**: ES Modules のロード、microtask、イベントループ、
  Promise と非同期 I/O の接続を、deno_core に頼らず自分の手で組む
- **マルチテナントの実行基盤**: 1 プロセスに多数のテナントを共存させる
  Isolate モデル、コールドスタート、LRU プール、リソース制限（CPU・メモリ）
- **Rust の実践**: ブロッキングな JS 実行と tokio の非同期世界を安全に分離する
  スレッディング設計、`unsafe` を最小限に閉じ込める方法

## 前提知識

- Rust の基本（所有権・ライフタイム・トレイト）
- JavaScript の基本（Promise / async-await / ES Modules）
- HTTP の基本

V8 の知識は前提にしません。第 II 部で必要なことをすべて説明します。

## 読み方

本書はプロジェクトのマイルストーン（M0〜M6）に沿って進みます。
各章は「何を作るか → なぜその設計か → コードの核心 → 動かして確かめる」
の順で書かれています。リポジトリのコードは各章の解説と対応しているので、
手元で `cargo run` しながら読むことを勧めます。

```sh
git clone https://github.com/sakurai-ryo/toy-edge-worker
cd toy-edge-worker
cargo run -p edged -- eval "1+2*3"   # => 7 と出れば準備完了
```

## 本書のビルド

```sh
cd docs && mdbook serve   # http://localhost:3000
```

## 関連プロジェクト

前作 [toy-lambda-runtime](https://github.com/sakurai-ryo/toy-lambda-runtime) は
Firecracker microVM（**プロセスより強い分離**）で AWS Lambda を再現しました。
本作はその対極、**プロセスより弱いが桁違いに軽い分離**である V8 Isolate を
選んだ世界を探検します。2 冊を並べて読むと、サーバーレス基盤の分離技術の
スペクトラムが見えてくるはずです。
