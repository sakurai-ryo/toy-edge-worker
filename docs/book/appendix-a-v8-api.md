# 付録 A: rusty_v8 API 早見表

本プロジェクトで実際に使った API の索引です（v8 = "149" 系で検証）。
一次資料は docs.rs と crate 同梱の `examples/`・`src/` です。

## 初期化・Isolate

| API | 用途 | 章 |
|---|---|---|
| `v8::new_default_platform(0, false).make_shared()` | プラットフォーム生成 | 5 |
| `v8::V8::initialize_platform(p)` / `v8::V8::initialize()` | プロセスで 1 回。`Once` で守る | 5 |
| `v8::Isolate::new(params) -> OwnedIsolate` | Isolate 生成。**自動で enter される** | 5 |
| `v8::CreateParams::default().heap_limits(init, max)` | ヒープ上限 | 14 |
| `isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit)` | microtask を手動 flush に | 8 |
| `isolate.perform_microtask_checkpoint()` | microtask を全部流す | 8 |
| `isolate.set_slot(value)` / `isolate.get_slot::<T>()` | 埋め込み側データの置き場 | 8 |
| `isolate.thread_safe_handle() -> IsolateHandle` | Send+Sync な制御ハンドル | 13 |
| `unsafe isolate.enter()` / `unsafe isolate.exit()` | カレント Isolate の切替（LIFO 厳守） | 11 |
| `isolate.add_near_heap_limit_callback(cb, data)` | ヒープ上限コールバック | 14 |

## IsolateHandle（他スレッドから）

| API | 用途 | 章 |
|---|---|---|
| `handle.terminate_execution() -> bool` | 実行中 JS の強制中断（catch 不能） | 13 |
| `isolate.cancel_terminate_execution()` | 中断フラグ解除 → Isolate 再利用可 | 13 |
| `isolate.is_execution_terminating()` | 中断処理中かの確認 | 8 |

## スコープ（マクロ）

| マクロ | 束縛される型（実用上） | 用途 |
|---|---|---|
| `v8::scope!(let s, &mut isolate)` | `&mut PinScope<'_, '_, ()>` | HandleScope |
| `v8::scope!(let s, ctx_scope)` | `&mut PinScope<'_, '_>`（C=Context） | Context 付きスコープ |
| `v8::ContextScope::new(scope, ctx)` | `ContextScope` | Context に入る |
| `v8::scope_with_context!(let s, iso, global_ctx)` | 〃 | `Global<Context>` から一発 |
| `v8::tc_scope!(let tc, scope)` | `&mut PinnedRef<TryCatch<...>>` | 例外捕捉。スコープとしても使える（Deref） |
| `v8::callback_scope!(unsafe s, context)` | `&mut PinScope` 相当 | V8 コールバック内で復元 |

関数の引数型は `&mut v8::PinScope`（または `&v8::PinScope`）で書く。

## ハンドル

| API | 用途 | 章 |
|---|---|---|
| `v8::Local<'s, T>` | スコープ縛りの短命ハンドル | 4 |
| `v8::Global::new(scope, local)` | スコープを越える長命ハンドル（GC ルート） | 4 |
| `v8::Local::new(scope, &global)` | Global → 現スコープの Local | 4 |
| `v8::Local::<T>::try_from(value)` | ダウンキャスト（`Result`） | 6 |

## 値の生成・変換

| API | 用途 |
|---|---|
| `v8::String::new(scope, &str) -> Option<Local<String>>` | Rust → JS 文字列 |
| `value.to_string(scope)` + `.to_rust_string_lossy(scope)` | JS → Rust 文字列 |
| `v8::Integer::new_from_unsigned(scope, u32)` | 数値 |
| `value.uint32_value(scope)` / `value.number_value(scope)` | JS → Rust 数値 |
| `v8::undefined(scope)` | undefined |
| `v8::Object::new(scope)` / `obj.set(scope, k, v)` / `obj.get(scope, k)` | オブジェクト。**get の「無い」は Some(undefined)** |
| `obj.delete(scope, key)` | プロパティ削除（__runtime 隠蔽） |
| `v8::Array::new(scope, len)` / `arr.set_index` / `arr.get_index` / `arr.length()` | 配列 |
| `v8::ArrayBuffer::new_backing_store_from_vec(vec)` + `with_backing_store` | `Vec<u8>` → ArrayBuffer（ゼロコピー） |
| `buffer.get_backing_store()` / `view.copy_contents(&mut buf)` | ArrayBuffer/View → バイト列 |
| `v8::Exception::error(scope, msg)` / `type_error` | 例外オブジェクト生成 |
| `scope.throw_exception(exc)` | 例外を投げる（コールバックから） |

## スクリプト・モジュール

| API | 用途 | 章 |
|---|---|---|
| `v8::Script::compile(scope, src, None)` + `script.run(scope)` | classic script（bootstrap） | 5,7 |
| `v8::ScriptOrigin::new(..., is_module: true, ...)` | ESM のオリジン情報 | 6 |
| `v8::script_compiler::Source::new(src, Some(&origin))` | コンパイル入力 | 6 |
| `v8::script_compiler::compile_module(scope, &mut source)` | ESM コンパイル | 6 |
| `module.instantiate_module(scope, resolve_cb)` | import 解決 | 6 |
| `module.evaluate(scope)` | 評価。**常に Promise が返る** | 6 |
| `module.get_status()` / `get_exception()` | Errored の検査 | 6 |
| `module.get_module_namespace()` | export の取り出し | 6 |

## 関数・Promise

| API | 用途 | 章 |
|---|---|---|
| `v8::Function::new(scope, callback)` | Rust 関数を JS Function に | 7 |
| callback 型: `fn(&mut PinScope, FunctionCallbackArguments, ReturnValue)` | キャプチャ不可 | 7 |
| `function.call(scope, recv, &args) -> Option<Local<Value>>` | JS 関数呼び出し | 9 |
| `v8::PromiseResolver::new(scope)` / `get_promise` / `resolve` / `reject` | Promise の Rust 側操作 | 8 |
| `promise.state()` (`Pending/Fulfilled/Rejected`) / `promise.result(scope)` | ポーリング | 8 |

## TryCatch

| API | 用途 |
|---|---|
| `tc.exception() -> Option<Local<Value>>` | 例外値 |
| `tc.has_terminated()` | terminate による中断か |
| `tc.can_continue()` | 続行可能か |

## エラー受けの基本形

```rust
v8::tc_scope!(let tc, scope);
let Some(result) = some_v8_call(tc, ...) else {
    if tc.has_terminated() { return Err(WorkerError::Terminated); }
    return Err(WorkerError::Script(exception_message!(tc)));
};
```
