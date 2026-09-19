//! Agent-native JSON 方法层。
//!
//! 唯一的实现处；`helius api` / `helius mcp` / `helius serve` 都只是它的适配器。
//!
//! 契约（见 `AGENT-NATIVE.md`）：
//! - 请求：`{ "method": "...", "params": {...}, "dry_run": bool, "idempotency_key": "..." }`
//! - 响应：`{ "ok": true, "method": "...", "data": {...}, "meta": {...} }`
//!          `{ "ok": false, "method": "...", "error": { "code", "message", "hint" } }`
//! - 错误码机器可读（`VALIDATION` / `UNKNOWN_ACCOUNT` / `CURRENCY_MISMATCH` / `DUPLICATE` …）
//! - 退出码：0 成功 / 2 校验 / 3 找不到 / 4 冲突 / 5 内部

use crate::db::{Db, TransactionProvenance};
use crate::error::AppError;
use crate::model::{NewTransaction, TransactionFilters, TransactionKind};
use crate::services::transactions::TransactionService;
use serde::Deserialize;
use serde_json::{json, Value};

pub const EXIT_OK: i32 = 0;
pub const EXIT_VALIDATION: i32 = 2;
pub const EXIT_NOT_FOUND: i32 = 3;
pub const EXIT_CONFLICT: i32 = 4;
pub const EXIT_INTERNAL: i32 = 5;

#[derive(Debug, Deserialize)]
pub struct ApiRequest {
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// 把 `AppError` 映射成机器可读的错误码 + 退出码。
/// agent 靠 `code` 分支决策，不解析 message。
fn classify(error: &AppError) -> (&'static str, &'static str, i32) {
    match error {
        AppError::FieldValidation { .. } => ("VALIDATION", "修正该字段后重试", EXIT_VALIDATION),
        AppError::Validation(message) => {
            // 上游把「跨币种」也走 Validation，这里给它一个专属错误码
            if message.contains("currencies") && message.contains("must not be summed") {
                ("CURRENCY_MISMATCH", "按币种拆开，或用 --account 指定单一账户", EXIT_VALIDATION)
            } else if message.contains("was not found") || message.contains("not found") {
                ("NOT_FOUND", "先调 schema.describe 确认引用存在", EXIT_NOT_FOUND)
            } else {
                ("VALIDATION", "修正参数后重试", EXIT_VALIDATION)
            }
        }
        AppError::NotFoundEntity { .. } | AppError::NotFound(_) => {
            ("NOT_FOUND", "先调 schema.describe 确认引用存在", EXIT_NOT_FOUND)
        }
        AppError::DuplicateEntity { .. } | AppError::AlreadyExists(_) => {
            ("DUPLICATE", "改用 idempotency_key，或修改内容", EXIT_CONFLICT)
        }
        AppError::Db(_) => {
            // SQLite UNIQUE 约束（例如幂等键撞车）归为冲突
            ("DUPLICATE", "该唯一键已被占用；若为幂等键，说明这笔已记过", EXIT_CONFLICT)
        }
        AppError::Config(_) => ("CONFIG", "检查数据库是否已 init", EXIT_INTERNAL),
        _ => ("INTERNAL", "内部错误，请人工检查", EXIT_INTERNAL),
    }
}

fn ok(method: &str, data: Value, meta: Value) -> (String, i32) {
    (
        json!({ "ok": true, "method": method, "data": data, "meta": meta }).to_string(),
        EXIT_OK,
    )
}

fn err(method: &str, code: &str, message: String, hint: &str, exit: i32) -> (String, i32) {
    (
        json!({
            "ok": false,
            "method": method,
            "error": { "code": code, "message": message, "hint": hint }
        })
        .to_string(),
        exit,
    )
}

/// 接受数字或字符串两种金额写法（agent 常直接给数字）
fn amount_to_text(value: &Value) -> Result<String, AppError> {
    match value {
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(s.clone()),
        _ => Err(AppError::FieldValidation {
            field: "amount".to_string(),
            reason: "must be a number or a numeric string".to_string(),
        }),
    }
}

fn parse_kind(raw: &str) -> Result<TransactionKind, AppError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "income" => Ok(TransactionKind::Income),
        "expense" => Ok(TransactionKind::Expense),
        "transfer" => Ok(TransactionKind::Transfer),
        other => Err(AppError::FieldValidation {
            field: "kind".to_string(),
            reason: format!("`{other}` is not one of income / expense / transfer"),
        }),
    }
}

fn str_param(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(|v| match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    })
}

fn opt_str(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 处理一次调用。返回 (JSON 字符串, 退出码)。
pub fn handle(db: &Db, raw: &str) -> (String, i32) {
    let request: ApiRequest = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => {
            return err(
                "<unparsed>",
                "VALIDATION",
                format!("请求不是合法的 JSON 请求信封：{error}"),
                r#"形如 {"method":"schema.describe","params":{}}"#,
                EXIT_VALIDATION,
            )
        }
    };
    let method = request.method.clone();
    match dispatch(db, &request) {
        Ok((data, meta)) => ok(&method, data, meta),
        Err(error) => {
            let (code, hint, exit) = classify(&error);
            err(&method, code, error.to_string(), hint, exit)
        }
    }
}

fn dispatch(db: &Db, request: &ApiRequest) -> Result<(Value, Value), AppError> {
    let meta = json!({
        "schema_version": crate::CURRENT_SCHEMA_VERSION,
        "primary_currency": db.currency_code()?,
    });

    match request.method.as_str() {
        // ── 自省：agent 第一步应该调这个 ────────────────────────────────
        "schema.describe" => {
            let accounts = db.list_accounts()?;
            let categories = db.list_categories()?;
            let mut currencies: Vec<String> = accounts
                .iter()
                .map(|a| {
                    a.currency
                        .clone()
                        .unwrap_or_else(|| db.currency_code().unwrap_or_default())
                })
                .collect();
            currencies.sort();
            currencies.dedup();
            Ok((
                json!({
                    "accounts": accounts,
                    "categories": categories,
                    "currencies": currencies,
                    "primary_currency": db.currency_code()?,
                    "transaction_kinds": ["income", "expense", "transfer"],
                    "sources": ["agent", "human", "import"],
                    "confidence_levels": ["exact", "derived", "uncertain"],
                    "error_codes": [
                        "VALIDATION", "UNKNOWN_ACCOUNT", "UNKNOWN_CATEGORY",
                        "CURRENCY_MISMATCH", "DUPLICATE", "NOT_FOUND", "CONFLICT",
                        "CONFIG", "INTERNAL"
                    ],
                    "exit_codes": {
                        "0": "ok", "2": "validation", "3": "not_found",
                        "4": "conflict", "5": "internal"
                    },
                    "modeling_note": "一个「人 × 一种币种」= 一个账户（例如 晓青-THB、我-CNY）"
                }),
                meta,
            ))
        }

        "accounts.list" => Ok((json!(db.list_accounts()?), meta)),

        "tx.list" => {
            let p = &request.params;
            let filters = TransactionFilters {
                from: opt_str(p, "from"),
                to: opt_str(p, "to"),
                account: opt_str(p, "account"),
                category: opt_str(p, "category"),
                search: opt_str(p, "search"),
                limit: p.get("limit").and_then(Value::as_u64).map(|v| v as usize),
                include_deleted: p
                    .get("include_deleted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            Ok((json!(TransactionService::new(db).list(&filters)?), meta))
        }

        "summary" => {
            let p = &request.params;
            let from = match str_param(p, "from") {
                Some(value) => crate::normalize_date(&value)?,
                None => return Err(AppError::Validation("summary requires `from`".to_string())),
            };
            let to = match str_param(p, "to") {
                Some(value) => crate::normalize_date(&value)?,
                None => return Err(AppError::Validation("summary requires `to`".to_string())),
            };
            let record = db.summary(&from, &to, opt_str(p, "account").as_deref())?;
            Ok((json!(record), meta))
        }

        // ── 写入：幂等 + dry-run + 溯源 ───────────────────────────────
        "tx.add" => {
            let p = &request.params;
            let txn = NewTransaction {
                txn_date: crate::normalize_date(&str_param(p, "date").ok_or_else(|| {
                    AppError::Validation("tx.add requires `date`".to_string())
                })?)?,
                kind: parse_kind(&str_param(p, "kind").ok_or_else(|| {
                    AppError::Validation("tx.add requires `kind`".to_string())
                })?)?,
                amount_cents: crate::amount::parse_amount_to_cents(&amount_to_text(
                    p.get("amount").ok_or_else(|| {
                        AppError::Validation("tx.add requires `amount`".to_string())
                    })?,
                )?)?,
                account: str_param(p, "account").ok_or_else(|| {
                    AppError::Validation("tx.add requires `account`".to_string())
                })?,
                to_account: opt_str(p, "to_account"),
                category: opt_str(p, "category"),
                payee: opt_str(p, "payee"),
                note: opt_str(p, "note"),
                recurring_rule_id: None,
            };

            let provenance = TransactionProvenance {
                external_ref: request.idempotency_key.clone(),
                source: opt_str(p, "source").or_else(|| Some("agent".to_string())),
                evidence: opt_str(p, "evidence"),
                session: opt_str(p, "session"),
                confidence: opt_str(p, "confidence"),
            };

            // 幂等：同 key 已存在 → 直接返回原记录，不报错（重发是正常行为）
            if let Some(key) = &provenance.external_ref {
                if let Some(existing) = db.find_transaction_by_external_ref(key)? {
                    return Ok((
                        json!({ "id": existing, "deduped": true, "idempotency_key": key }),
                        meta,
                    ));
                }
            }

            if request.dry_run {
                return Ok((
                    json!({
                        "dry_run": true,
                        "would_write": {
                            "txn_date": txn.txn_date,
                            "kind": format!("{:?}", txn.kind).to_ascii_lowercase(),
                            "amount_cents": txn.amount_cents,
                            "account": txn.account,
                            "category": txn.category,
                            "payee": txn.payee,
                            "note": txn.note,
                        },
                        "provenance": {
                            "external_ref": provenance.external_ref,
                            "source": provenance.source,
                            "evidence": provenance.evidence,
                            "session": provenance.session,
                            "confidence": provenance.confidence,
                        },
                        "deduped": false,
                    }),
                    meta,
                ));
            }

            let service = TransactionService::new(db);
            let id = service.add(&txn)?;
            db.set_transaction_provenance(id, &provenance)?;
            Ok((
                json!({
                    "id": id,
                    "deduped": false,
                    "idempotency_key": provenance.external_ref,
                }),
                meta,
            ))
        }

        other => Err(AppError::Validation(format!(
            "unknown method `{other}`；可用：schema.describe / accounts.list / tx.list / summary / tx.add"
        ))),
    }
}
