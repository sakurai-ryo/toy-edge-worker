mod server;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use protocol::WorkerRequest;
use runtime::{Worker, WorkerConfig};

#[derive(Parser)]
#[command(name = "tew", about = "toy edge worker runtime")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// JS 式を評価して結果を表示する
    Eval { code: String },
    /// worker スクリプトを 1 回だけ呼び出して Response を表示する
    Invoke {
        /// worker スクリプト（ES Modules 形式）のパス
        script: PathBuf,
        /// リクエストのパス（または絶対 URL）
        #[arg(long, default_value = "/")]
        url: String,
        #[arg(long, default_value = "GET")]
        method: String,
        /// リクエストボディ
        #[arg(long)]
        body: Option<String>,
        /// リクエストヘッダ（"key: value" 形式、複数指定可）
        #[arg(long = "header")]
        headers: Vec<String>,
        /// env に注入する変数（"KEY=VALUE" 形式、複数指定可）
        #[arg(long = "env")]
        env: Vec<String>,
    },
    /// worker スクリプトを HTTP サーバとして公開する
    Serve {
        /// テナント設定ファイル（workers.toml）
        #[arg(long, default_value = "workers.toml")]
        config: PathBuf,
        /// 単一スクリプトモード（--config の代わりに 1 worker を "/" で公開）
        #[arg(long, conflicts_with = "config")]
        script: Option<PathBuf>,
        /// listen アドレスの上書き
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// env に注入する変数（"KEY=VALUE" 形式、複数指定可。--script モード専用）
        #[arg(long = "env")]
        env: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Eval { code } => {
            println!("{}", runtime::eval(&code)?);
        }
        Command::Invoke {
            script,
            url,
            method,
            body,
            headers,
            env,
        } => invoke(script, url, method, body, headers, env)?,
        Command::Serve {
            config,
            script,
            listen,
            env,
        } => {
            let options = build_serve_options(config, script, listen, env)?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(server::serve(options))?;
        }
    }
    Ok(())
}

fn build_serve_options(
    config_path: PathBuf,
    script: Option<PathBuf>,
    listen: Option<SocketAddr>,
    env: Vec<String>,
) -> Result<server::ServeOptions> {
    use router::{Route, WorkerDef};
    use server::TenantSpec;
    use std::collections::HashMap;
    use std::time::Duration;

    // --script モードは「path = "/" の単一テナント」として扱う
    let (server_listen, worker_threads, limits, workers) = if let Some(script) = script {
        let workers = vec![WorkerDef {
            name: "default".into(),
            script,
            route: Route {
                host: None,
                path: Some("/".into()),
            },
            env: parse_env(&env)?.into_iter().collect(),
        }];
        ("127.0.0.1:8787".to_string(), 1, router::Limits::default(), workers)
    } else {
        let config = router::load_config(&config_path)?;
        // スクリプトパスは workers.toml からの相対
        let base = config_path.parent().unwrap_or_else(|| ".".as_ref());
        let workers = config
            .workers
            .into_iter()
            .map(|mut def| {
                if def.script.is_relative() {
                    def.script = base.join(&def.script);
                }
                def
            })
            .collect();
        (
            config.server.listen,
            config.server.worker_threads,
            config.limits,
            workers,
        )
    };

    let route_table = router::RouteTable::from_workers(&workers);

    let mut tenants = HashMap::new();
    for def in &workers {
        let script_source = std::fs::read_to_string(&def.script)
            .with_context(|| format!("failed to read {}", def.script.display()))?;
        let script_url = format!("file://{}", def.script.canonicalize()?.display());
        tenants.insert(
            def.name.clone(),
            TenantSpec {
                script_source,
                script_url,
                env: def.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                wall_timeout: Duration::from_millis(limits.wall_timeout_ms),
                cpu_budget: Some(Duration::from_millis(limits.cpu_ms)),
                heap_limit: Some(limits.heap_mb * 1024 * 1024),
            },
        );
    }

    let listen = match listen {
        Some(listen) => listen,
        None => server_listen
            .parse()
            .with_context(|| format!("invalid listen address: {server_listen}"))?,
    };

    Ok(server::ServeOptions {
        listen,
        worker_threads,
        isolates_per_thread: (limits.isolates_max / worker_threads.max(1)).max(1),
        route_table,
        tenants,
    })
}

fn parse_env(env: &[String]) -> Result<Vec<(String, String)>> {
    env.iter()
        .map(|pair| {
            let (key, value) = pair
                .split_once('=')
                .with_context(|| format!("invalid --env (expected KEY=VALUE): {pair}"))?;
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

fn invoke(
    script: PathBuf,
    url: String,
    method: String,
    body: Option<String>,
    headers: Vec<String>,
    env: Vec<String>,
) -> Result<()> {
    let source = std::fs::read_to_string(&script)
        .with_context(|| format!("failed to read {}", script.display()))?;
    let script_url = format!("file://{}", script.canonicalize()?.display());

    let env = parse_env(&env)?;

    let headers = headers
        .iter()
        .map(|pair| {
            let (key, value) = pair
                .split_once(':')
                .with_context(|| format!("invalid --header (expected 'key: value'): {pair}"))?;
            Ok((key.trim().to_string(), value.trim().to_string()))
        })
        .collect::<Result<Vec<_>>>()?;

    let url = if url.starts_with('/') {
        format!("http://localhost{url}")
    } else {
        url
    };

    // worker 内 fetch() 用の tokio ランタイム（invoke が終わるまで生かす）
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let config = WorkerConfig {
        env,
        tokio: Some(tokio_runtime.handle().clone()),
        ..Default::default()
    };
    let mut worker = Worker::new(&source, &script_url, config)?;

    let request = WorkerRequest {
        method: method.to_uppercase(),
        url,
        headers,
        body: body.map(String::into_bytes).unwrap_or_default(),
    };
    let response = worker.handle(&request)?;

    println!("HTTP {}", response.status);
    for (key, value) in &response.headers {
        println!("{key}: {value}");
    }
    println!();
    println!("{}", String::from_utf8_lossy(&response.body));
    Ok(())
}
