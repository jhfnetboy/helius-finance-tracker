//! `helius serve` 的端到端测试：真的起一个服务，真的发 HTTP 请求。
//!
//! 固定的是"人类入口"的承诺：页面能开、JSON 能取、**只读**。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};

fn helius_bin() -> std::path::PathBuf {
    // 集成测试的二进制就在同一个 target 目录里
    let mut path = std::env::current_exe().expect("exe");
    path.pop(); // deps/
    path.pop(); // debug|release/
    path.push("helius");
    path
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

fn seed(path: &Path) {
    let db = helius::Db::open_for_init(path).expect("open_for_init");
    db.init("CNY").expect("init");
    drop(db);
    let db = helius::Db::open_existing(path).expect("open_existing");
    let accounts = helius::services::accounts::AccountService::new(&db);
    for (name, currency, owner) in [("我-CNY", "CNY", "我"), ("晓青-THB", "THB", "晓青")] {
        accounts
            .add(helius::services::accounts::AddAccountRequest {
                name: name.to_string(),
                kind: helius::AccountKind::Checking,
                opening_balance_cents: 0,
                opened_on: "2026-01-01".to_string(),
                currency: Some(currency.to_string()),
                owner: Some(owner.to_string()),
            })
            .expect("account");
    }
}

fn get(port: u16, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").expect("write");
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).expect("status");
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("header") == 0 {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = value.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).expect("body");
    (status, String::from_utf8_lossy(&body).to_string())
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(db: &Path, port: u16) -> Server {
    let child = Command::new(helius_bin())
        .args(["--db", db.to_str().expect("db path"), "serve", "--port", &port.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");
    // 等它就绪：重试连接
    for _ in 0..60 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server(child);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("serve did not come up on port {port}");
}

#[test]
fn serve_returns_the_page_and_json_apis() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let db_path = temp.path().join("tracker.db");
    seed(&db_path);
    let port = free_port();
    let _server = start(&db_path, port);

    let (status, body) = get(port, "/");
    assert!(status.contains("200"), "首页状态：{status}");
    assert!(body.contains("记账看板"), "首页应含标题");
    // 页面通过 fetch('/api/' + name) 取数，字面量不会整串出现
    assert!(body.contains("fetch('/api/'"), "页面应调用 JSON 接口");
    assert!(body.contains("按人的账"), "页面应渲染按人分组的账");

    let (status, body) = get(port, "/api/overview");
    assert!(status.contains("200"), "overview 状态：{status}");
    let overview: serde_json::Value = serde_json::from_str(&body).expect("overview json");
    assert_eq!(overview["primary_currency"], "CNY");
    let accounts = overview["accounts"].as_array().expect("accounts");
    assert_eq!(accounts.len(), 2);
    // 每个账户都带自己的币种 —— 页面据此分桶，不会跨币种相加
    let currencies: Vec<&str> = accounts
        .iter()
        .filter_map(|a| a["currency"].as_str())
        .collect();
    assert!(currencies.contains(&"CNY") && currencies.contains(&"THB"), "{currencies:?}");

    let (status, body) = get(port, "/api/transactions");
    assert!(status.contains("200"));
    assert!(serde_json::from_str::<serde_json::Value>(&body).is_ok());

    let (status, body) = get(port, "/api/questions");
    assert!(status.contains("200"));
    assert!(serde_json::from_str::<serde_json::Value>(&body).is_ok());

    let (status, _) = get(port, "/nope");
    assert!(status.contains("404"), "未知路径应 404，实际：{status}");
}

#[test]
fn serve_is_read_only() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let db_path = temp.path().join("tracker.db");
    seed(&db_path);
    let port = free_port();
    let _server = start(&db_path, port);

    // 制造一次读
    let _ = get(port, "/api/overview");

    // 库里的记录数不该因为看板而改变
    let db = helius::Db::open_existing(&db_path).expect("open");
    let filters = helius::TransactionFilters {
        from: None,
        to: None,
        account: None,
        category: None,
        search: None,
        limit: None,
        include_deleted: false,
    };
    assert_eq!(db.list_transactions(&filters).expect("txns").len(), 0);
}
