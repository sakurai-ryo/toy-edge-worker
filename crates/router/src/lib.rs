//! workers.toml のパースと Host/Path ルーティング。
//!
//! マッチング規則:
//! 1. Host ヘッダ（ポート除去後）の完全一致
//! 2. パスの最長プレフィックス一致
//! 3. どちらも不一致なら None（HTTP 404）

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub workers: Vec<WorkerDef>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_worker_threads")]
    pub worker_threads: usize,
}

/// リソース制限のデフォルト値。worker 個別の上書きは未対応（必要になったら追加）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// CPU 時間制限（M5。v1 は wall-clock で近似）
    pub cpu_ms: u64,
    /// V8 ヒープ上限（M5）
    pub heap_mb: usize,
    /// プロセス全体の warm isolate 上限（M4。スレッドに均等分配）
    pub isolates_max: usize,
    /// リクエスト全体の wall-clock タイムアウト
    pub wall_timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cpu_ms: 50,
            heap_mb: 64,
            isolates_max: 16,
            wall_timeout_ms: 10_000,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WorkerDef {
    pub name: String,
    pub script: PathBuf,
    pub route: Route,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct Route {
    pub host: Option<String>,
    pub path: Option<String>,
}

fn default_listen() -> String {
    "127.0.0.1:8787".into()
}

fn default_worker_threads() -> usize {
    4
}

pub fn load_config(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(config)
}

/// テナント解決テーブル。フロントが Host/Path からテナント名を引く。
#[derive(Debug, Default)]
pub struct RouteTable {
    host_routes: HashMap<String, String>,
    /// (prefix, tenant)。最長一致のため prefix 長の降順でソート済み
    path_routes: Vec<(String, String)>,
}

impl RouteTable {
    pub fn from_workers(workers: &[WorkerDef]) -> Self {
        let mut table = RouteTable::default();
        for def in workers {
            if let Some(host) = &def.route.host {
                table.host_routes.insert(host.clone(), def.name.clone());
            }
            if let Some(path) = &def.route.path {
                table.path_routes.push((path.clone(), def.name.clone()));
            }
        }
        table
            .path_routes
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        table
    }

    /// Host ヘッダ（ポート付きでもよい）とパスからテナント名を解決する。
    pub fn resolve(&self, host: &str, path: &str) -> Option<&str> {
        let host = host.split(':').next().unwrap_or(host);
        if let Some(tenant) = self.host_routes.get(host) {
            return Some(tenant);
        }
        self.path_routes
            .iter()
            .find(|(prefix, _)| path.starts_with(prefix.as_str()))
            .map(|(_, tenant)| tenant.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, host: Option<&str>, path: Option<&str>) -> WorkerDef {
        WorkerDef {
            name: name.into(),
            script: PathBuf::from(format!("{name}.js")),
            route: Route {
                host: host.map(Into::into),
                path: path.map(Into::into),
            },
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn host_exact_match_wins() {
        let table = RouteTable::from_workers(&[
            def("hello", Some("hello.localhost"), None),
            def("echo", None, Some("/")),
        ]);
        assert_eq!(table.resolve("hello.localhost", "/anything"), Some("hello"));
        assert_eq!(table.resolve("hello.localhost:8787", "/x"), Some("hello"));
        assert_eq!(table.resolve("other.localhost", "/x"), Some("echo"));
    }

    #[test]
    fn path_longest_prefix() {
        let table = RouteTable::from_workers(&[
            def("api", None, Some("/api/")),
            def("api-v2", None, Some("/api/v2/")),
            def("root", None, Some("/")),
        ]);
        assert_eq!(table.resolve("localhost", "/api/v2/users"), Some("api-v2"));
        assert_eq!(table.resolve("localhost", "/api/users"), Some("api"));
        assert_eq!(table.resolve("localhost", "/top"), Some("root"));
    }

    #[test]
    fn no_match_is_none() {
        let table = RouteTable::from_workers(&[def("api", None, Some("/api/"))]);
        assert_eq!(table.resolve("localhost", "/other"), None);
    }

    #[test]
    fn parse_config() {
        let config: Config = toml::from_str(
            r#"
            [server]
            listen = "127.0.0.1:9999"

            [limits]
            cpu_ms = 100

            [[workers]]
            name = "hello"
            script = "hello.js"
            route = { host = "hello.localhost" }
            env = { GREETING = "hi" }
            "#,
        )
        .unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:9999");
        assert_eq!(config.server.worker_threads, 4);
        assert_eq!(config.limits.cpu_ms, 100);
        assert_eq!(config.limits.heap_mb, 64);
        assert_eq!(config.workers[0].env["GREETING"], "hi");
    }
}
