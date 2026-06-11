//! hyper の HTTP フロントエンドと worker スレッド群の配線。
//!
//! JS の実行（ブロッキング）は tokio ランタイムに載せず、専用の OS スレッドで
//! 行う。フロントとは mpsc(WorkItem) + oneshot(レスポンス) でだけ会話する。
//!
//! テナントは hash(tenant) % worker_threads で固定スレッドに割り当てる。
//! Isolate は生成したスレッドから外に出さない（スレッド間に渡すのは
//! チャネルのメッセージだけ）。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use pool::{Metrics, Pool};
use protocol::{WorkerRequest, WorkerResponse};
use router::RouteTable;
use runtime::{Worker, WorkerConfig, WorkerError};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// テナント 1 つ分の実行仕様。worker スレッドが cold start 時に参照する。
pub struct TenantSpec {
    pub script_source: String,
    pub script_url: String,
    pub env: Vec<(String, String)>,
    pub wall_timeout: Duration,
    pub cpu_budget: Option<Duration>,
    pub heap_limit: Option<usize>,
}

pub struct ServeOptions {
    pub listen: SocketAddr,
    pub worker_threads: usize,
    /// スレッドあたりの warm isolate 上限（isolates_max / worker_threads）
    pub isolates_per_thread: usize,
    pub route_table: RouteTable,
    pub tenants: HashMap<String, TenantSpec>,
}

/// worker スレッドへの依頼 1 件。
pub struct WorkItem {
    pub tenant: String,
    pub request: WorkerRequest,
    pub reply: oneshot::Sender<Result<WorkerResponse, WorkerError>>,
}

/// worker スレッドへのメッセージ。
pub enum Msg {
    Work(WorkItem),
    #[allow(dead_code)] // graceful shutdown は未配線
    Shutdown,
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let tenants = Arc::new(options.tenants);
    let route_table = Arc::new(options.route_table);
    let metrics = Arc::new(Metrics::default());

    // worker 内 fetch() の I/O はこの tokio ランタイムで実行する
    let tokio_handle = tokio::runtime::Handle::current();

    let worker_threads = options.worker_threads.max(1);
    let senders: Vec<mpsc::Sender<Msg>> = (0..worker_threads)
        .map(|i| {
            let (tx, rx) = mpsc::channel::<Msg>();
            let tenants = tenants.clone();
            let metrics = metrics.clone();
            let capacity = options.isolates_per_thread;
            let tokio_handle = tokio_handle.clone();
            std::thread::Builder::new()
                .name(format!("worker-{i}"))
                .spawn(move || worker_thread(i, rx, tenants, capacity, metrics, tokio_handle))
                .context("failed to spawn worker thread")?;
            Ok(tx)
        })
        .collect::<Result<_>>()?;
    let senders = Arc::new(senders);

    let listener = TcpListener::bind(options.listen)
        .await
        .with_context(|| format!("failed to bind {}", options.listen))?;
    println!("listening on http://{}", options.listen);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let route_table = route_table.clone();
        let senders = senders.clone();

        let metrics = metrics.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let route_table = route_table.clone();
                let senders = senders.clone();
                let metrics = metrics.clone();
                async move { handle_http(req, route_table, senders, metrics).await }
            });
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                eprintln!("connection error: {err}");
            }
        });
    }
}

/// worker スレッド本体。担当テナントの Worker(Isolate) を LRU プールで所有し、
/// リクエストを順番に処理する（1 スレッド = 同時実行 1 リクエスト）。
fn worker_thread(
    index: usize,
    rx: mpsc::Receiver<Msg>,
    tenants: Arc<HashMap<String, TenantSpec>>,
    capacity: usize,
    metrics: Arc<Metrics>,
    tokio_handle: tokio::runtime::Handle,
) {
    let mut workers: Pool<Worker> = Pool::new(capacity);

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Work(item) => {
                let tenant = &item.tenant;
                metrics.with(tenant, |m| m.requests += 1);

                // warm hit か cold start か
                if workers.get(tenant).is_some() {
                    metrics.with(tenant, |m| m.warm_hits += 1);
                } else {
                    match cold_start(index, &tenants, tenant, &metrics, &tokio_handle) {
                        Ok(worker) => {
                            if let Some((evicted, _)) = workers.insert(tenant.clone(), worker) {
                                metrics.with(&evicted, |m| m.evictions += 1);
                                println!(
                                    "[worker-{index}] evicted tenant={evicted} (LRU, capacity={capacity})"
                                );
                            }
                        }
                        Err(err) => {
                            metrics.with(tenant, |m| m.errors += 1);
                            let _ = item.reply.send(Err(err));
                            continue;
                        }
                    }
                }

                let worker = workers.get(tenant).expect("worker just inserted");
                let result = worker.handle(&item.request);
                match &result {
                    Err(WorkerError::CpuExceeded) => {
                        metrics.with(tenant, |m| {
                            m.errors += 1;
                            m.terminations_cpu += 1;
                        });
                        println!("[worker-{index}] cpu limit hit tenant={tenant} (isolate reused)");
                    }
                    Err(WorkerError::MemExceeded) => {
                        metrics.with(tenant, |m| {
                            m.errors += 1;
                            m.terminations_mem += 1;
                        });
                        // ヒープが膨らんだ isolate は再利用せず破棄（condemned）
                        workers.remove(tenant);
                        println!("[worker-{index}] heap limit hit tenant={tenant} (isolate discarded)");
                    }
                    Err(_) => metrics.with(tenant, |m| m.errors += 1),
                    Ok(_) => {}
                }
                // 受信側が先に消えていても無視してよい
                let _ = item.reply.send(result);
            }
            Msg::Shutdown => break,
        }
    }
}

fn cold_start(
    thread_index: usize,
    tenants: &HashMap<String, TenantSpec>,
    tenant: &str,
    metrics: &Metrics,
    tokio_handle: &tokio::runtime::Handle,
) -> Result<Worker, WorkerError> {
    let spec = tenants
        .get(tenant)
        .ok_or_else(|| WorkerError::Internal(format!("unknown tenant: {tenant}")))?;
    let started = Instant::now();
    let worker = Worker::new(
        &spec.script_source,
        &spec.script_url,
        WorkerConfig {
            env: spec.env.clone(),
            wall_timeout: spec.wall_timeout,
            cpu_budget: spec.cpu_budget,
            heap_limit: spec.heap_limit,
            tokio: Some(tokio_handle.clone()),
        },
    )?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    metrics.with(tenant, |m| m.record_cold_start(elapsed_ms));
    println!("[worker-{thread_index}] cold start tenant={tenant} ({elapsed_ms:.1}ms)");
    Ok(worker)
}

async fn handle_http(
    req: Request<Incoming>,
    route_table: Arc<RouteTable>,
    senders: Arc<Vec<mpsc::Sender<Msg>>>,
    metrics: Arc<Metrics>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // ルーティング（Host 完全一致 → path 最長プレフィックス）
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let path = req.uri().path().to_string();

    if path == "/__admin/metrics" {
        let json = serde_json::to_string_pretty(&metrics.snapshot()).unwrap_or_default();
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(json)))
            .unwrap());
    }
    let Some(tenant) = route_table.resolve(&host, &path).map(str::to_string) else {
        return Ok(plain_response(
            StatusCode::NOT_FOUND,
            &format!("no worker matched host={host} path={path}"),
        ));
    };

    let request = match collect_request(req, &host).await {
        Ok(request) => request,
        Err(err) => return Ok(plain_response(StatusCode::BAD_REQUEST, &err.to_string())),
    };

    // テナント → スレッド固定（同一テナントの Isolate は常に同じスレッドに住む）
    let sender = {
        let mut hasher = DefaultHasher::new();
        tenant.hash(&mut hasher);
        &senders[(hasher.finish() % senders.len() as u64) as usize]
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    let item = WorkItem {
        tenant,
        request,
        reply: reply_tx,
    };
    if sender.send(Msg::Work(item)).is_err() {
        return Ok(plain_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "worker thread is gone",
        ));
    }

    match reply_rx.await {
        Ok(Ok(response)) => {
            let mut builder = Response::builder().status(response.status);
            for (key, value) in &response.headers {
                builder = builder.header(key, value);
            }
            Ok(builder
                .body(Full::new(Bytes::from(response.body)))
                .unwrap_or_else(|_| {
                    plain_response(StatusCode::INTERNAL_SERVER_ERROR, "invalid response")
                }))
        }
        Ok(Err(err)) => {
            let status = match err {
                WorkerError::Timeout
                | WorkerError::Terminated
                | WorkerError::CpuExceeded
                | WorkerError::MemExceeded => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            eprintln!("worker error: {err}");
            Ok(plain_response(status, &err.to_string()))
        }
        Err(_) => Ok(plain_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "worker dropped the request",
        )),
    }
}

/// hyper の Request から body を全て集めて protocol::WorkerRequest を作る。
async fn collect_request(req: Request<Incoming>, host: &str) -> Result<WorkerRequest> {
    let method = req.method().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string();
    let url = format!("http://{host}{path_and_query}");

    let headers = req
        .headers()
        .iter()
        .filter_map(|(key, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (key.as_str().to_string(), v.to_string()))
        })
        .collect();

    let body = req.into_body().collect().await?.to_bytes().to_vec();

    Ok(WorkerRequest {
        method,
        url,
        headers,
        body,
    })
}

fn plain_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(format!("{message}\n"))))
        .unwrap()
}
