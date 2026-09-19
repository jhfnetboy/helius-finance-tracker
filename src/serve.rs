//! `helius serve` —— 本地只读看板（人类入口）。
//!
//! 设计要点：
//! - **手写 HTTP/1.1**，只用 `std::net` + 已有的 `serde_json`，不引新依赖 ——
//!   保持"单二进制"这个卖点。
//! - **只监听 127.0.0.1**，**只读**：绝不修改数据库，也不发起任何外部请求。
//! - 与 `helius api` / `helius mcp` 共用同一套方法层语义（同一份数据、同一套币种规则）。

use crate::db::Db;
use crate::error::AppError;
use crate::model::TransactionFilters;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread;

/// 看板页面。单独放 assets 里，避免在 Rust 里转义 HTML。
const PAGE: &str = include_str!("../assets/serve.html");

/// 汇总用的"全时间"区间 —— 只是为了让上游 API 开心，实际覆盖所有历史。
const FROM_ALL: &str = "1900-01-01";
const TO_ALL: &str = "2999-12-31";

pub fn run(db_path: &Path, port: u16) -> Result<(), AppError> {
    if !db_path.exists() {
        return Err(AppError::Config(format!(
            "数据库不存在：{}；先跑 `helius --db {} init --currency CNY`",
            db_path.display(),
            db_path.display()
        )));
    }

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let bound = listener.local_addr()?;
    println!("记账看板  →  http://127.0.0.1:{}", bound.port());
    println!("数据文件  →  {}", db_path.display());
    println!("只读服务，仅监听 127.0.0.1；Ctrl-C 退出。");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let path = db_path.to_path_buf();
        thread::spawn(move || {
            let _ = handle(stream, &path);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, db_path: &Path) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;

    // 只读请求行足够；这个服务不需要 body。
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let route = path.split('?').next().unwrap_or("/");

    match route {
        "/" | "/index.html" => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE),
        "/api/overview" => match overview(db_path) {
            Ok(value) => respond(&mut stream, "200 OK", JSON, &value.to_string()),
            Err(error) => fail(&mut stream, error),
        },
        "/api/transactions" => match transactions(db_path) {
            Ok(value) => respond(&mut stream, "200 OK", JSON, &value.to_string()),
            Err(error) => fail(&mut stream, error),
        },
        "/api/questions" => match questions(db_path) {
            Ok(value) => respond(&mut stream, "200 OK", JSON, &value.to_string()),
            Err(error) => fail(&mut stream, error),
        },
        _ => respond(&mut stream, "404 Not Found", "text/plain; charset=utf-8", "not found"),
    }
}

const JSON: &str = "application/json; charset=utf-8";

fn fail(stream: &mut TcpStream, error: AppError) -> std::io::Result<()> {
    let body = json!({ "ok": false, "error": error.to_string() }).to_string();
    respond(stream, "500 Internal Server Error", JSON, &body)
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.as_bytes().len()
    )?;
    stream.flush()
}

fn open(db_path: &Path) -> Result<Db, AppError> {
    Db::open_existing(db_path)
}

/// 每个账户的收/支/净。**按账户隔离币种**，所以永远不会跨币种相加。
fn account_rows(db: &Db) -> Result<Vec<Value>, AppError> {
    let mut rows = Vec::new();
    for account in db.list_accounts()? {
        let summary = db.summary(FROM_ALL, TO_ALL, Some(&account.name))?;
        rows.push(json!({
            "name": account.name,
            "owner": account.owner,
            "currency": summary.currency.or(account.currency),
            "kind": account.kind.as_db_str(),
            "income_cents": summary.income_cents,
            "expense_cents": summary.expense_cents,
            "net_cents": summary.net_cents,
            "transactions": summary.transaction_count,
        }));
    }
    Ok(rows)
}

fn overview(db_path: &Path) -> Result<Value, AppError> {
    let db = open(db_path)?;

    let recurring: Vec<Value> = db
        .list_recurring_rules()?
        .into_iter()
        .map(|rule| {
            json!({
                "name": rule.name,
                "account": rule.account_name,
                "category": rule.category_name,
                "amount_cents": rule.amount_cents,
                "day_of_month": rule.day_of_month,
                "cadence": format!("{:?}", rule.cadence).to_ascii_lowercase(),
                "next_due_on": rule.next_due_on,
            })
        })
        .collect();

    Ok(json!({
        "generated_at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "schema_version": crate::CURRENT_SCHEMA_VERSION,
        "primary_currency": db.currency_code()?,
        "accounts": account_rows(&db)?,
        "recurring": recurring,
        "open_questions": db.list_questions(true)?.len(),
    }))
}

fn transactions(db_path: &Path) -> Result<Value, AppError> {
    let db = open(db_path)?;
    let filters = TransactionFilters {
        from: None,
        to: None,
        account: None,
        category: None,
        search: None,
        limit: Some(300),
        include_deleted: false,
    };
    let rows: Vec<Value> = db
        .list_transactions(&filters)?
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id,
                "date": t.txn_date,
                "kind": format!("{:?}", t.kind).to_ascii_lowercase(),
                "amount_cents": t.amount_cents,
                "account": t.account_name,
                "category": t.category_name,
                "note": t.note,
            })
        })
        .collect();

    // 溯源单独取一次 —— 上游的 TransactionRecord 还不含这些列
    let with_provenance: Vec<Value> = {
        let mut out = Vec::new();
        for row in rows {
            let id = row["id"].as_i64().unwrap_or_default();
            let provenance = db.transaction_provenance(id)?;
            let mut merged = row;
            if let Some(p) = provenance {
                merged["external_ref"] = json!(p.external_ref);
                merged["source"] = json!(p.source);
                merged["evidence"] = json!(p.evidence);
                merged["confidence"] = json!(p.confidence);
            }
            out.push(merged);
        }
        out
    };

    Ok(json!(with_provenance))
}

fn questions(db_path: &Path) -> Result<Value, AppError> {
    let db = open(db_path)?;
    Ok(json!(db.list_questions(false)?))
}

