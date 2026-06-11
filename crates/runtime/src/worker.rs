//! Worker 本体: Isolate の生成、ESM ロード、fetch ハンドラ呼び出し、
//! そして deno_core の run_event_loop 相当のミニイベントループ。

use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{WorkerRequest, WorkerResponse};

use crate::error::WorkerError;
use crate::limits::{self, CpuMeter, MemCtx};
use crate::ops::{self, LoopEvent, OpResult, OpState};

const BOOTSTRAP_JS: &str = include_str!("../js/bootstrap.js");

pub struct WorkerConfig {
    /// fetch(request, env, ctx) の env に注入する文字列変数
    pub env: Vec<(String, String)>,
    /// リクエスト 1 件あたりの wall-clock タイムアウト
    pub wall_timeout: Duration,
    /// リクエスト 1 件あたりの CPU 予算（JS 実行スライスの合計時間で近似）。
    /// None なら無制限
    pub cpu_budget: Option<Duration>,
    /// V8 ヒープ上限（バイト）。None なら無制限
    pub heap_limit: Option<usize>,
    /// サブリクエスト fetch の I/O を実行する tokio ランタイム。
    /// None なら worker 内の fetch() は使えない
    pub tokio: Option<tokio::runtime::Handle>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            env: Vec::new(),
            wall_timeout: Duration::from_secs(10),
            cpu_budget: None,
            heap_limit: None,
            tokio: None,
        }
    }
}

/// 1 つの worker スクリプトを評価済みの Isolate。
///
/// 1 スレッドで複数の Worker（= Isolate）を共存させるため、Isolate は
/// 「使うときだけ enter し、使い終わったら exit する」方式で管理する
/// （V8 の enter/exit はスレッドローカルなスタックのため、entered のまま
/// 別の Isolate に触ってはならない）。new() の終わりで exit して park し、
/// handle() の間だけ enter する。
///
/// フィールドの宣言順が重要: Global ハンドル群は isolate より先に
/// drop されなければならないため、isolate を最後に宣言している。
pub struct Worker {
    context: v8::Global<v8::Context>,
    fetch_handler: v8::Global<v8::Function>,
    build_request: v8::Global<v8::Function>,
    serialize_response: v8::Global<v8::Function>,
    env: v8::Global<v8::Object>,
    op_rx: mpsc::Receiver<LoopEvent>,
    wall_timeout: Duration,
    cpu_budget: Option<Duration>,
    isolate_handle: v8::IsolateHandle,
    /// near-heap-limit コールバックが立てるフラグ（terminate の原因分類用）
    mem_hit: Arc<AtomicBool>,
    isolate: v8::OwnedIsolate,
    /// near-heap-limit コールバックに渡したデータ。V8 が isolate 破棄まで
    /// 参照するため、isolate より後に drop されるようここに置く
    _mem_ctx: Option<Box<MemCtx>>,
}

impl Worker {
    /// スクリプトを ESM としてロード・評価し、default export の fetch を取り出す。
    pub fn new(script: &str, script_url: &str, config: WorkerConfig) -> Result<Self, WorkerError> {
        crate::init();

        let mut params = v8::CreateParams::default();
        if let Some(limit) = config.heap_limit {
            params = params.heap_limits(0, limit);
        }
        let mut isolate = v8::Isolate::new(params);
        // microtask は イベントループが明示的に流す
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);

        let isolate_handle = isolate.thread_safe_handle();
        let mem_hit = Arc::new(AtomicBool::new(false));
        let mem_ctx = config.heap_limit.map(|_| {
            let ctx = Box::new(MemCtx {
                handle: isolate_handle.clone(),
                hit: mem_hit.clone(),
            });
            isolate.add_near_heap_limit_callback(
                limits::near_heap_limit_callback,
                &*ctx as *const MemCtx as *mut c_void,
            );
            ctx
        });

        let (tx, op_rx) = mpsc::channel();
        isolate.set_slot(Rc::new(RefCell::new(OpState::new(tx, config.tokio.clone()))));

        let (context, fetch_handler, build_request, serialize_response, env) = {
            v8::scope!(let hs, &mut isolate);
            let context = v8::Context::new(hs, Default::default());
            let context_global = v8::Global::new(hs, context);
            let cs = &mut v8::ContextScope::new(hs, context);
            v8::scope!(let scope, cs);

            ops::install_ops(scope);
            run_script(scope, BOOTSTRAP_JS, "tew:bootstrap.js")?;
            let (build_request, serialize_response) = take_runtime_helpers(scope)?;
            let env = build_env(scope, &config.env);

            // モジュール評価（top-level コード）にも CPU 予算を適用する
            let mut meter = CpuMeter::new(isolate_handle.clone(), config.cpu_budget);
            let module = compile_and_instantiate(scope, script, script_url)?;
            evaluate_module(scope, &op_rx, &module, config.wall_timeout, &mut meter)?;
            let fetch_handler = extract_fetch_handler(scope, &module)?;

            (
                context_global,
                fetch_handler,
                build_request,
                serialize_response,
                env,
            )
        };

        // park: 同一スレッドの他の Isolate を使えるように current から外す
        unsafe { isolate.exit() };

        Ok(Self {
            context,
            fetch_handler,
            build_request,
            serialize_response,
            env,
            op_rx,
            wall_timeout: config.wall_timeout,
            cpu_budget: config.cpu_budget,
            isolate_handle,
            mem_hit,
            isolate,
            _mem_ctx: mem_ctx,
        })
    }

    /// HTTP リクエスト 1 件を fetch ハンドラに通して Response を取り出す。
    pub fn handle(&mut self, req: &WorkerRequest) -> Result<WorkerResponse, WorkerError> {
        // SAFETY: Worker は生成スレッドからのみ使われ、enter と exit が対になる。
        // （panic 時は exit が漏れるが、worker スレッドごと落とす前提の toy 実装）
        unsafe { self.isolate.enter() };
        let mut result = self.handle_inner(req);
        if matches!(result, Err(WorkerError::Terminated)) {
            result = Err(self.classify_termination());
        }
        unsafe { self.isolate.exit() };
        result
    }

    /// terminate の原因（CPU か メモリか）を分類し、isolate を片付ける。
    fn classify_termination(&self) -> WorkerError {
        // terminate フラグを下ろす（CPU 超過なら isolate を再利用できる）
        self.isolate.cancel_terminate_execution();
        // 未完了の op は破棄（遅れて届く完了通知は op_id 不一致で無視される）
        ops::get_op_state(&self.isolate).borrow_mut().abort_all();
        if self.mem_hit.load(Ordering::SeqCst) {
            WorkerError::MemExceeded
        } else {
            WorkerError::CpuExceeded
        }
    }

    fn handle_inner(&mut self, req: &WorkerRequest) -> Result<WorkerResponse, WorkerError> {
        let deadline = Instant::now() + self.wall_timeout;
        let mut meter = CpuMeter::new(self.isolate_handle.clone(), self.cpu_budget);

        v8::scope!(let hs, &mut self.isolate);
        let context = v8::Local::new(hs, &self.context);
        let cs = &mut v8::ContextScope::new(hs, context);
        v8::scope!(let scope, cs);

        // (1) Rust の WorkerRequest から JS の Request を構築（bootstrap ヘルパ経由）
        let js_request = {
            v8::tc_scope!(let tc, scope);
            let build = v8::Local::new(tc, &self.build_request);
            let url: v8::Local<v8::Value> = v8::String::new(tc, &req.url).unwrap().into();
            let method: v8::Local<v8::Value> = v8::String::new(tc, &req.method).unwrap().into();
            let headers: v8::Local<v8::Value> = headers_to_js(tc, &req.headers).into();
            let body: v8::Local<v8::Value> = if req.body.is_empty() {
                v8::undefined(tc).into()
            } else {
                ops::bytes_to_array_buffer(tc, req.body.clone()).into()
            };
            let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
            let Some(js_request) = meter.run(|| build.call(tc, recv, &[url, method, headers, body]))
            else {
                return Err(WorkerError::Internal(format!(
                    "buildRequest failed: {}",
                    exception_message!(tc)
                )));
            };
            v8::Global::new(tc, js_request)
        };

        // (2) fetch(request, env, ctx) を呼び、返り値を Promise に正規化
        let result_promise = {
            v8::tc_scope!(let tc, scope);
            let handler = v8::Local::new(tc, &self.fetch_handler);
            let js_request = v8::Local::new(tc, &js_request);
            let env: v8::Local<v8::Value> = v8::Local::new(tc, &self.env).into();
            let ctx: v8::Local<v8::Value> = v8::Object::new(tc).into();
            let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
            let Some(ret) = meter.run(|| handler.call(tc, recv, &[js_request, env, ctx])) else {
                if tc.has_terminated() {
                    return Err(WorkerError::Terminated);
                }
                return Err(WorkerError::Script(exception_message!(tc)));
            };
            let promise = match v8::Local::<v8::Promise>::try_from(ret) {
                Ok(promise) => promise,
                Err(_) => {
                    // 同期で Response を返すハンドラも許容する（Promise.resolve 相当）
                    let resolver = v8::PromiseResolver::new(tc).unwrap();
                    resolver.resolve(tc, ret);
                    resolver.get_promise(tc)
                }
            };
            v8::Global::new(tc, promise)
        };

        // (3) Promise 解決までイベントループを回す
        let response =
            run_event_loop_until_settled(scope, &self.op_rx, &result_promise, deadline, &mut meter)?;

        // (4) Response をシリアライズ（body 読み出しが async なので再度ループ）
        let serialize_promise = {
            v8::tc_scope!(let tc, scope);
            let serialize = v8::Local::new(tc, &self.serialize_response);
            let response = v8::Local::new(tc, &response);
            let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
            let Some(ret) = meter.run(|| serialize.call(tc, recv, &[response])) else {
                if tc.has_terminated() {
                    return Err(WorkerError::Terminated);
                }
                return Err(WorkerError::Script(exception_message!(tc)));
            };
            let promise = v8::Local::<v8::Promise>::try_from(ret).map_err(|_| {
                WorkerError::Internal("serializeResponse did not return a promise".into())
            })?;
            v8::Global::new(tc, promise)
        };
        let triple = run_event_loop_until_settled(
            scope,
            &self.op_rx,
            &serialize_promise,
            deadline,
            &mut meter,
        )?;

        // (5) [status, headers, body] を Rust の値へ
        let triple = v8::Local::new(scope, &triple);
        let triple = v8::Local::<v8::Array>::try_from(triple)
            .map_err(|_| WorkerError::Internal("serializeResponse returned a non-array".into()))?;
        let status = triple
            .get_index(scope, 0)
            .and_then(|v| v.uint32_value(scope))
            .unwrap_or(200) as u16;
        let headers = triple
            .get_index(scope, 1)
            .map(|v| js_headers_to_vec(scope, v))
            .unwrap_or_default();
        let body = triple
            .get_index(scope, 2)
            .and_then(ops::value_to_bytes)
            .unwrap_or_default();

        Ok(WorkerResponse {
            status,
            headers,
            body,
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // OwnedIsolate の Drop は自分が current であることを要求する。
        // park 状態（exit 済み）なので enter してから drop に進む。
        // Global 群はフィールド宣言順により isolate より先に破棄される。
        unsafe { self.isolate.enter() };
    }
}

/// classic script を評価する（bootstrap 用）。
fn run_script(scope: &mut v8::PinScope, code: &str, name: &str) -> Result<(), WorkerError> {
    v8::tc_scope!(let tc, scope);
    let source = v8::String::new(tc, code)
        .ok_or_else(|| WorkerError::Internal(format!("{name}: source too long")))?;
    let Some(script) = v8::Script::compile(tc, source, None) else {
        return Err(WorkerError::Internal(format!(
            "{name}: compile error: {}",
            exception_message!(tc)
        )));
    };
    if script.run(tc).is_none() {
        return Err(WorkerError::Internal(format!(
            "{name}: {}",
            exception_message!(tc)
        )));
    }
    Ok(())
}

/// ESM を compile → instantiate する。import 解決は未対応（M1 時点）。
fn compile_and_instantiate(
    scope: &mut v8::PinScope,
    code: &str,
    url: &str,
) -> Result<v8::Global<v8::Module>, WorkerError> {
    v8::tc_scope!(let tc, scope);
    let source_str = v8::String::new(tc, code)
        .ok_or_else(|| WorkerError::Internal("script too long".into()))?;
    let name: v8::Local<v8::Value> = v8::String::new(tc, url).unwrap().into();
    let origin = v8::ScriptOrigin::new(
        tc, name, 0, 0, false, 0, None, false, false, /* is_module */ true, None,
    );
    let mut source = v8::script_compiler::Source::new(source_str, Some(&origin));

    let Some(module) = v8::script_compiler::compile_module(tc, &mut source) else {
        return Err(WorkerError::Script(exception_message!(tc)));
    };
    if module.instantiate_module(tc, resolve_module_callback).is_none() {
        return Err(WorkerError::Script(exception_message!(tc)));
    }
    Ok(v8::Global::new(tc, module))
}

/// import 解決コールバック。M1 では import 自体を未対応としてエラーにする。
/// （将来: ModuleMap を isolate slot に置き、ここでは登録済みモジュールを引くだけにする）
fn resolve_module_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    _referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    // SAFETY: V8 のモジュール解決中に呼ばれるため context は有効
    v8::callback_scope!(unsafe scope, context);
    let specifier = specifier.to_rust_string_lossy(scope);
    let message =
        v8::String::new(scope, &format!("import is not supported yet: {specifier}")).unwrap();
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
    None
}

/// module.evaluate() し、返ってくる Promise（top-level await 対応のため常に
/// Promise が返る）の解決をイベントループで待つ。
fn evaluate_module(
    scope: &mut v8::PinScope,
    op_rx: &mpsc::Receiver<LoopEvent>,
    module: &v8::Global<v8::Module>,
    timeout: Duration,
    meter: &mut CpuMeter,
) -> Result<(), WorkerError> {
    let deadline = Instant::now() + timeout;
    let promise = {
        v8::tc_scope!(let tc, scope);
        let module = v8::Local::new(tc, module);
        let Some(value) = meter.run(|| module.evaluate(tc)) else {
            if tc.has_terminated() {
                return Err(WorkerError::Terminated);
            }
            return Err(WorkerError::Script(exception_message!(tc)));
        };
        let promise = v8::Local::<v8::Promise>::try_from(value).map_err(|_| {
            WorkerError::Internal("module evaluate did not return a promise".into())
        })?;
        v8::Global::new(tc, promise)
    };

    match run_event_loop_until_settled(scope, op_rx, &promise, deadline, meter) {
        Ok(_) => Ok(()),
        Err(err) => {
            // Errored の場合はモジュール側の例外を優先して報告する
            let module = v8::Local::new(scope, module);
            if module.get_status() == v8::ModuleStatus::Errored {
                let exception = module.get_exception();
                return Err(WorkerError::Script(format_exception(scope, exception)));
            }
            Err(err)
        }
    }
}

/// module namespace から default export の fetch を取り出す。
fn extract_fetch_handler(
    scope: &mut v8::PinScope,
    module: &v8::Global<v8::Module>,
) -> Result<v8::Global<v8::Function>, WorkerError> {
    let module = v8::Local::new(scope, module);
    let namespace = v8::Local::<v8::Object>::try_from(module.get_module_namespace())
        .map_err(|_| WorkerError::Internal("module namespace is not an object".into()))?;

    let key = v8::String::new(scope, "default").unwrap();
    let default = namespace
        .get(scope, key.into())
        .filter(|v| !v.is_undefined())
        .ok_or_else(|| WorkerError::Script("script has no default export".into()))?;
    let default = v8::Local::<v8::Object>::try_from(default)
        .map_err(|_| WorkerError::Script("default export is not an object".into()))?;

    let key = v8::String::new(scope, "fetch").unwrap();
    let fetch = default
        .get(scope, key.into())
        .filter(|v| !v.is_undefined())
        .ok_or_else(|| WorkerError::Script("default export has no fetch handler".into()))?;
    let fetch = v8::Local::<v8::Function>::try_from(fetch)
        .map_err(|_| WorkerError::Script("fetch handler is not a function".into()))?;

    Ok(v8::Global::new(scope, fetch))
}

/// bootstrap.js が globalThis.__runtime に置いたヘルパを取り出し、
/// ユーザーコードから見えないように delete する。
fn take_runtime_helpers(
    scope: &mut v8::PinScope,
) -> Result<(v8::Global<v8::Function>, v8::Global<v8::Function>), WorkerError> {
    let context = scope.get_current_context();
    let global = context.global(scope);

    let runtime_key = v8::String::new(scope, "__runtime").unwrap();
    let runtime = global
        .get(scope, runtime_key.into())
        .and_then(|v| v8::Local::<v8::Object>::try_from(v).ok())
        .ok_or_else(|| WorkerError::Internal("__runtime not found after bootstrap".into()))?;

    let take = |name: &str| -> Result<v8::Global<v8::Function>, WorkerError> {
        let key = v8::String::new(scope, name).unwrap();
        let func = runtime
            .get(scope, key.into())
            .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok())
            .ok_or_else(|| WorkerError::Internal(format!("__runtime.{name} not found")))?;
        Ok(v8::Global::new(scope, func))
    };
    let build_request = take("buildRequest")?;
    let serialize_response = take("serializeResponse")?;

    global.delete(scope, runtime_key.into());
    Ok((build_request, serialize_response))
}

fn build_env(scope: &mut v8::PinScope, env: &[(String, String)]) -> v8::Global<v8::Object> {
    let obj = v8::Object::new(scope);
    for (key, value) in env {
        let key = v8::String::new(scope, key).unwrap();
        let value = v8::String::new(scope, value).unwrap();
        obj.set(scope, key.into(), value.into());
    }
    v8::Global::new(scope, obj)
}

/// deno_core の run_event_loop 相当。target の Promise が settle するまで
/// microtask checkpoint と pending op の完了待ちを繰り返す。
fn run_event_loop_until_settled(
    scope: &mut v8::PinScope,
    op_rx: &mpsc::Receiver<LoopEvent>,
    target: &v8::Global<v8::Promise>,
    deadline: Instant,
    meter: &mut CpuMeter,
) -> Result<v8::Global<v8::Value>, WorkerError> {
    let state = ops::get_op_state(scope);
    loop {
        // (1) resolve 済み Promise の .then / await 継続をまとめて実行
        //     （JS 実行スライスなので CPU 予算の監視下に置く）
        meter.run(|| scope.perform_microtask_checkpoint());

        // (2) terminate（CPU/メモリ制限。M5）チェック
        if scope.is_execution_terminating() {
            state.borrow_mut().abort_all();
            return Err(WorkerError::Terminated);
        }

        // (3) 対象 Promise の状態確認
        let promise = v8::Local::new(scope, target);
        match promise.state() {
            v8::PromiseState::Fulfilled => {
                let value = promise.result(scope);
                return Ok(v8::Global::new(scope, value));
            }
            v8::PromiseState::Rejected => {
                let exception = promise.result(scope);
                return Err(WorkerError::Script(format_exception(scope, exception)));
            }
            v8::PromiseState::Pending => {}
        }

        // (4) 進行中の op が無いのに pending なら、永遠に解決しない
        if state.borrow().pending_ops == 0 {
            return Err(WorkerError::Hung);
        }

        // (5) op 完了待ち = イベントループの待機点
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            state.borrow_mut().abort_all();
            return Err(WorkerError::Timeout);
        }
        match op_rx.recv_timeout(timeout) {
            Ok(LoopEvent::OpComplete { op_id, result }) => {
                complete_op(scope, &state, op_id, result);
                // 溜まっている完了通知を非ブロッキングで吸い出す
                while let Ok(LoopEvent::OpComplete { op_id, result }) = op_rx.try_recv() {
                    complete_op(scope, &state, op_id, result);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                state.borrow_mut().abort_all();
                return Err(WorkerError::Timeout);
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(WorkerError::Internal("op channel disconnected".into()));
            }
        }
    }
}

/// op の完了を対応する PromiseResolver に反映する。
fn complete_op(
    scope: &mut v8::PinScope,
    state: &Rc<RefCell<OpState>>,
    op_id: u64,
    result: Result<OpResult, String>,
) {
    let resolver = {
        let mut st = state.borrow_mut();
        let Some(resolver) = st.resolvers.remove(&op_id) else {
            // terminate で破棄済みの op が遅れて完了した場合
            return;
        };
        st.pending_ops -= 1;
        resolver
    };
    let resolver = v8::Local::new(scope, &resolver);

    match result {
        Ok(OpResult::Timer) => {
            let value: v8::Local<v8::Value> = v8::undefined(scope).into();
            resolver.resolve(scope, value);
        }
        Ok(OpResult::Fetch {
            status,
            headers,
            body,
        }) => {
            // { status, headers: [[k,v],...], body: ArrayBuffer } を作って resolve
            let obj = v8::Object::new(scope);
            let key = v8::String::new(scope, "status").unwrap();
            let value = v8::Integer::new_from_unsigned(scope, status as u32);
            obj.set(scope, key.into(), value.into());
            let key = v8::String::new(scope, "headers").unwrap();
            let value = headers_to_js(scope, &headers);
            obj.set(scope, key.into(), value.into());
            let key = v8::String::new(scope, "body").unwrap();
            let value = ops::bytes_to_array_buffer(scope, body);
            obj.set(scope, key.into(), value.into());
            resolver.resolve(scope, obj.into());
        }
        Err(message) => {
            let message = v8::String::new(scope, &message).unwrap();
            let exception = v8::Exception::type_error(scope, message);
            resolver.reject(scope, exception);
        }
    }
}

/// 例外値を人間可読な文字列にする。Error オブジェクトなら stack を優先。
pub(crate) fn format_exception(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> String {
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(value) {
        let key = v8::String::new(scope, "stack").unwrap();
        if let Some(stack) = obj.get(scope, key.into()) {
            if stack.is_string() {
                return stack
                    .to_string(scope)
                    .map(|s| s.to_rust_string_lossy(scope))
                    .unwrap_or_default();
            }
        }
    }
    value
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "unknown error".into())
}

fn headers_to_js<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    headers: &[(String, String)],
) -> v8::Local<'s, v8::Array> {
    let array = v8::Array::new(scope, headers.len() as i32);
    for (i, (key, value)) in headers.iter().enumerate() {
        let pair = v8::Array::new(scope, 2);
        let key: v8::Local<v8::Value> = v8::String::new(scope, key).unwrap().into();
        let value: v8::Local<v8::Value> = v8::String::new(scope, value).unwrap().into();
        pair.set_index(scope, 0, key);
        pair.set_index(scope, 1, value);
        array.set_index(scope, i as u32, pair.into());
    }
    array
}

fn js_headers_to_vec(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Vec<(String, String)> {
    let Ok(array) = v8::Local::<v8::Array>::try_from(value) else {
        return Vec::new();
    };
    let mut headers = Vec::with_capacity(array.length() as usize);
    for i in 0..array.length() {
        let Some(pair) = array.get_index(scope, i) else {
            continue;
        };
        let Ok(pair) = v8::Local::<v8::Array>::try_from(pair) else {
            continue;
        };
        let text = |index: u32| {
            pair.get_index(scope, index)
                .and_then(|v| v.to_string(scope))
                .map(|s| s.to_rust_string_lossy(scope))
        };
        if let (Some(key), Some(value)) = (text(0), text(1)) {
            headers.push((key, value));
        }
    }
    headers
}
