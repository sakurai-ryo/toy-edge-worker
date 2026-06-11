# Cloudflare Workers と Isolate モデル

ランタイムを作り始める前に、「Isolate で分離する」とはどういうことか、
なぜ Cloudflare がそれを選んだのかを整理します。

## V8 Isolate とは何か

V8 は Chrome と Node.js に搭載されている JavaScript エンジンです。
その V8 において **Isolate（アイソレート）は「独立した JS 実行宇宙」の単位**です。

- Isolate は自分専用のヒープを持つ。GC も Isolate 単位で動く
- Isolate A のオブジェクトを Isolate B から参照することはできない。
  ポインタを渡しても無意味（そもそも API がそれを許さない）
- グローバルオブジェクト（`globalThis`）も Isolate（正確には後述の
  Context）ごとに別物

ブラウザでは「タブごと・iframe ごと」に Isolate や Context が割り当てられ、
悪意あるページが他のタブのデータを読めないようにしています。つまり Isolate は
**もともと「信頼できないコード同士を同じプロセスで動かす」ために設計された
分離単位**です。Cloudflare はこれをサーバ側のマルチテナントに転用しました。

### Isolate と Context

V8 の分離には実は 2 段階あります。

- **Isolate**: ヒープと GC の単位。スレッドとの対応もここで管理される
- **Context**: グローバルオブジェクトの単位。1 つの Isolate の中に複数の
  Context を作れる（同一 iframe ツリーなど、ある程度信頼し合う者同士の分離）

本プロジェクトでは **1 テナント = 1 Isolate（+ その中に 1 Context）** とします。
ヒープ自体を分けることで、メモリ制限や「壊れたら Isolate ごと捨てる」運用が
テナント単位でできるからです。Cloudflare Workers も 1 worker = 1 Isolate です。

## プロセス分離との比較

| | プロセス/microVM 分離 (Lambda) | Isolate 分離 (Workers) |
|---|---|---|
| 分離の主体 | カーネル（+ ハイパーバイザ） | V8 のサンドボックス + ランタイム設計 |
| コールドスタート | 数十 ms〜数百 ms | **1ms 未満** |
| テナントあたりメモリ | 数十 MB〜（ゲスト OS 込み） | **数 MB**（ヒープのみ） |
| 1 ホストの集積度 | 数百 | **数千〜数万** |
| 任意のバイナリ実行 | できる | できない（JS/Wasm のみ） |
| Spectre 級の攻撃への耐性 | 強い（アドレス空間が別） | 弱い。ランタイム側の対策が必須 |

コールドスタートの差は決定的です。エッジでは「世界中の数百 PoP すべてに
全テナントを置く」必要があり、リクエストが来てから VM を起動していては
間に合わず、かといって全テナント分の VM を常駐させるメモリもありません。
「来た瞬間に 1ms で起動、使い終わったら即捨てられる」Isolate だけが
この要件を満たします。

### 弱い分離をどう補うか

同一プロセス・同一アドレス空間である以上、Isolate 分離には固有のリスクが
あります。本家 workerd が施している対策と、本プロジェクトでの扱いは:

| リスク | 本家の対策 | 本プロジェクト |
|---|---|---|
| 無限ループで CPU を独占 | CPU 時間制限 + terminate | **M5 で実装**（watchdog） |
| メモリを食い尽くす | ヒープ上限 + Isolate 破棄 | **M5 で実装**（heap limit） |
| Spectre 系のサイドチャネル | `Date.now()` の粗粒度化、タイマー制限、プロセス再配置など | スコープ外（設計の議論のみ） |
| V8 自体の脆弱性 | 迅速なパッチ適用、プロセスレベルの多層防御 | スコープ外 |

「toy」と本物の距離が最も大きいのがこの表の下半分です。Isolate モデルの
安全性は V8 のサンドボックス**だけ**に依存してはならず、多層防御が前提に
なっています。

## 「ランタイムが API を与える」という安全設計

もう 1 つの本質は、**素の V8 には何もない**ということです。

素の Isolate + Context で使えるのは ECMAScript の言語機能だけです。
`console.log` も `setTimeout` も `fetch` も存在しません。ファイルアクセスも
ネットワークも、**埋め込み側（ランタイム）が明示的に関数を生やさない限り**
JS からは一切触れません。

これはセキュリティ的には理想的な出発点です。デフォルトが「何もできない」
なので、ランタイムが生やした API の総体 = 攻撃面、と明確になります。
Workers が `fetch` はあるのに `fs` がないのは、まさにこの設計の現れです。

本書では第 7 章で `console` / `Request` / `Response` などを生やし、
第 15 章で `fetch` を生やします。「API を 1 つ生やすとはどういう作業か」を
体感すると、この設計の意味がよく分かります。

## Cloudflare Workers の実行モデル（本プロジェクトが模倣する範囲）

本家から借りた概念は次のとおりです。

1. **ES Modules 形式のハンドラ**
   `export default { fetch(request, env, ctx) }`。Service Worker 形式
   （`addEventListener("fetch", ...)`）は旧式なので採らない。
2. **Isolate は使い回す（warm）**
   リクエストごとに Isolate を作るのではなく、一度作った Isolate を
   メモリが許す限り使い回す。最初の 1 回だけが cold start。
   このため「同一 worker の連続リクエストにはグローバル変数の状態が
   見える」ことがある — これは本家でも明文化された挙動で、本書でも
   そのまま再現されます。
3. **await 中は CPU 時間を消費しない**
   Workers の CPU 制限は「JS を実行している時間」だけを数える。
   外部 fetch を待つ時間は無料。第 13 章でこの会計を実装する。
4. **メモリ超過した Isolate は破棄**
   次のリクエストは cold start からやり直し。

逆に、本家にあって本プロジェクトにないもの（ストリーミング、KV や
Durable Objects などのバインディング、Wasm、cron トリガ…）は
第 16 章の発展課題に整理しています。

## 参考文献

- Kenton Varda, "Cloud Computing without Containers" (Cloudflare Blog)
  — Isolate モデルの原典的な解説
- [workerd](https://github.com/cloudflare/workerd) — 本家ランタイム
  （C++）。設計ドキュメントが充実している
- [V8 公式: Getting started with embedding](https://v8.dev/docs/embed)
  — Isolate / Context / Handle の一次資料
