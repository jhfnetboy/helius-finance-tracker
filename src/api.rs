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

/// 方法层内部的失败类型：既能承载普通 `AppError`，
/// 也能承载带 `details` 的富错误（批量操作要指出是哪一行出的问题）。
enum ApiFailure {
    Plain(AppError),
    Detailed {
        code: &'static str,
        message: String,
        hint: &'static str,
        exit: i32,
        details: Value,
    },
}

impl From<AppError> for ApiFailure {
    fn from(error: AppError) -> Self {
        ApiFailure::Plain(error)
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

/// 带 `details` 的错误信封：批量操作失败时用来指出**具体是哪一行**。
fn err_with_details(
    method: &str,
    code: &str,
    message: String,
    hint: &str,
    exit: i32,
    details: Value,
) -> (String, i32) {
    (
        json!({
            "ok": false,
            "method": method,
            "error": { "code": code, "message": message, "hint": hint, "details": details }
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

/// 从参数里构造一笔流水 + 它的溯源信息。
/// `tx.add` 与 `tx.batch` 共用同一套解析 —— 保证两条路径语义完全一致。
fn build_transaction(
    params: &Value,
    idempotency_key: Option<String>,
    method: &str,
) -> Result<(NewTransaction, TransactionProvenance), AppError> {
    let required = |key: &str| {
        str_param(params, key).ok_or_else(|| {
            AppError::Validation(format!("{method} requires `{key}`"))
        })
    };
    let txn = NewTransaction {
        txn_date: crate::normalize_date(&required("date")?)?,
        kind: parse_kind(&required("kind")?)?,
        amount_cents: crate::amount::parse_amount_to_cents(&amount_to_text(
            params
                .get("amount")
                .ok_or_else(|| AppError::Validation(format!("{method} requires `amount`")))?,
        )?)?,
        account: required("account")?,
        to_account: opt_str(params, "to_account"),
        category: opt_str(params, "category"),
        payee: opt_str(params, "payee"),
        note: opt_str(params, "note"),
        recurring_rule_id: None,
    };
    let provenance = TransactionProvenance {
        external_ref: idempotency_key,
        source: opt_str(params, "source").or_else(|| Some("agent".to_string())),
        evidence: opt_str(params, "evidence"),
        session: opt_str(params, "session"),
        confidence: opt_str(params, "confidence"),
    };
    Ok((txn, provenance))
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
        Err(ApiFailure::Plain(error)) => {
            let (code, hint, exit) = classify(&error);
            err(&method, code, error.to_string(), hint, exit)
        }
        Err(ApiFailure::Detailed {
            code,
            message,
            hint,
            exit,
            details,
        }) => err_with_details(&method, code, message, hint, exit, details),
    }
}

fn dispatch(db: &Db, request: &ApiRequest) -> Result<(Value, Value), ApiFailure> {
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
                    "methods": [
                        "schema.describe", "accounts.list", "tx.list", "summary",
                        "tx.add", "tx.batch",
                        "questions.ask", "questions.list", "questions.answer"
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
                None => return Err(AppError::Validation("summary requires `from`".to_string()).into()),
            };
            let to = match str_param(p, "to") {
                Some(value) => crate::normalize_date(&value)?,
                None => return Err(AppError::Validation("summary requires `to`".to_string()).into()),
            };
            let record = db.summary(&from, &to, opt_str(p, "account").as_deref())?;
            Ok((json!(record), meta))
        }

        // ── 写入：幂等 + dry-run + 溯源 ───────────────────────────────
        "tx.add" => {
            let p = &request.params;
            let (txn, provenance) =
                build_transaction(p, request.idempotency_key.clone(), "tx.add")?;

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

        // ── A3 批量原子：一张截图 = 一次提交，要么全进要么全不进 ────────
        "tx.batch" => {
            let p = &request.params;
            let items = p
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| AppError::Validation("tx.batch requires an `items` array".to_string()))?;
            if items.is_empty() {
                return Err(AppError::Validation("`items` must not be empty".to_string()).into());
            }

            // ① 形状解析：任何一行不合法 → 整体拒绝，一行都不写
            let mut shape_errors: Vec<Value> = Vec::new();
            let mut built: Vec<(NewTransaction, TransactionProvenance)> = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let key = opt_str(item, "idempotency_key");
                match build_transaction(item, key, "tx.batch item") {
                    Ok(pair) => built.push(pair),
                    Err(error) => shape_errors.push(json!({
                        "index": index,
                        "error": { "code": classify(&error).0, "message": error.to_string() }
                    })),
                }
            }
            if !shape_errors.is_empty() {
                let (code, hint, exit) = classify(&AppError::Validation(String::new()));
                return Err(ApiFailure::Detailed {
                    code,
                    message: format!(
                        "{} of {} items failed validation; **nothing was written**",
                        shape_errors.len(),
                        items.len()
                    ),
                    hint,
                    exit,
                    details: Value::Array(shape_errors),
                });
            }

            // ② 幂等：已记过的行直接算 deduped，不进写入集
            let mut results: Vec<Value> = Vec::new();
            let mut pending: Vec<(usize, NewTransaction, TransactionProvenance)> = Vec::new();
            let mut deduped = 0usize;
            for (index, (txn, provenance)) in built.into_iter().enumerate() {
                match provenance.external_ref.as_deref() {
                    Some(key) => match db.find_transaction_by_external_ref(key)? {
                        Some(existing) => {
                            deduped += 1;
                            results.push(json!({
                                "index": index, "status": "deduped", "id": existing,
                                "idempotency_key": key
                            }));
                        }
                        None => pending.push((index, txn, provenance)),
                    },
                    None => pending.push((index, txn, provenance)),
                }
            }

            // ③ dry-run：只回报将要写入什么
            if request.dry_run {
                let mut plan: Vec<Value> = results.clone();
                for (index, txn, _) in &pending {
                    plan.push(json!({
                        "index": index,
                        "status": "would_insert",
                        "txn_date": txn.txn_date,
                        "kind": format!("{:?}", txn.kind).to_ascii_lowercase(),
                        "amount_cents": txn.amount_cents,
                        "account": txn.account,
                        "category": txn.category,
                    }));
                }
                plan.sort_by_key(|entry| entry["index"].as_u64().unwrap_or(0));
                return Ok((
                    json!({
                        "dry_run": true,
                        "total": items.len(),
                        "would_insert": pending.len(),
                        "deduped": deduped,
                        "results": plan,
                    }),
                    meta,
                ));
            }

            // ④ 原子写入
            if pending.is_empty() {
                return Ok((
                    json!({
                        "total": items.len(), "inserted": 0, "deduped": deduped,
                        "failed": 0, "committed": true, "results": results,
                    }),
                    meta,
                ));
            }
            let batch: Vec<(NewTransaction, TransactionProvenance)> = pending
                .iter()
                .map(|(_, txn, provenance)| (txn.clone(), provenance.clone()))
                .collect();

            match db.add_transactions_atomic(&batch) {
                Ok(ids) => {
                    for ((index, _, _), id) in pending.iter().zip(ids.iter()) {
                        results.push(json!({ "index": index, "status": "inserted", "id": id }));
                    }
                    results.sort_by_key(|entry| entry["index"].as_u64().unwrap_or(0));
                    Ok((
                        json!({
                            "total": items.len(),
                            "inserted": ids.len(),
                            "deduped": deduped,
                            "failed": 0,
                            "committed": true,
                            "results": results,
                        }),
                        meta,
                    ))
                }
                Err((failed_index, error)) => {
                    // 回滚已发生；把库内下标换回请求里的下标再回报
                    let request_index = pending
                        .get(failed_index)
                        .map(|(index, _, _)| *index)
                        .unwrap_or(failed_index);
                    let (code, hint, exit) = classify(&error);
                    Err(ApiFailure::Detailed {
                        code,
                        message: format!(
                            "batch aborted at item {request_index}: {error}; **nothing was written**"
                        ),
                        hint,
                        exit,
                        details: json!([{
                            "index": request_index,
                            "error": { "message": error.to_string() }
                        }]),
                    })
                }
            }
        }

        // ── A6 待确认是一等公民：判不准就挂号，绝不猜 ──────────────────
        "questions.ask" => {
            let p = &request.params;
            let question = str_param(p, "question")
                .ok_or_else(|| AppError::Validation("questions.ask requires `question`".to_string()))?;
            let id = db.ask_question(
                opt_str(p, "scope").as_deref(),
                &question,
                opt_str(p, "impact").as_deref(),
                opt_str(p, "asked_by").as_deref().or(Some("agent")),
                opt_str(p, "session").as_deref(),
            )?;
            Ok((json!({ "id": id, "status": "open", "question": question }), meta))
        }

        "questions.list" => {
            let open_only = request
                .params
                .get("open_only")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Ok((json!(db.list_questions(open_only)?), meta))
        }

        "questions.answer" => {
            let p = &request.params;
            let id = p
                .get("id")
                .and_then(|v| match v {
                    Value::Number(n) => n.as_i64(),
                    Value::String(s) => s.trim().parse::<i64>().ok(),
                    _ => None,
                })
                .ok_or_else(|| AppError::Validation("questions.answer requires numeric `id`".to_string()))?;
            let answer = str_param(p, "answer")
                .ok_or_else(|| AppError::Validation("questions.answer requires `answer`".to_string()))?;
            db.answer_question(id, &answer)?;
            Ok((json!({ "id": id, "status": "closed", "answer": answer }), meta))
        }

        other => Err(AppError::Validation(format!(
            "unknown method `{other}`；可用：schema.describe / accounts.list / tx.list / summary / \
             tx.add / tx.batch / questions.ask / questions.list / questions.answer"
        ))
        .into()),
    }
}
