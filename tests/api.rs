//! `helius api` —— agent-native 方法层的契约测试。
//!
//! 这里固定的是**给 agent 的承诺**（见 AGENT-NATIVE.md）：
//! 响应信封形状、幂等语义、dry-run 不落库、错误码与退出码。

use helius::api::{self, EXIT_CONFLICT, EXIT_NOT_FOUND, EXIT_OK, EXIT_VALIDATION};
use helius::services::accounts::{AccountService, AddAccountRequest};
use helius::{AccountKind, CategoryKind, Db, TransactionFilters};
use serde_json::Value;
use tempfile::TempDir;

fn fresh_db() -> (TempDir, Db) {
    let temp_dir = TempDir::new().expect("tempdir");
    let path = temp_dir.path().join("tracker.db");
    let db = Db::open_for_init(&path).expect("open_for_init");
    db.init("CNY").expect("init");
    drop(db);
    let db = Db::open_existing(&path).expect("open_existing");
    (temp_dir, db)
}

fn seed(db: &Db) {
    let service = AccountService::new(db);
    for (name, currency, owner) in [("我-CNY", "CNY", "我"), ("晓青-THB", "THB", "晓青")] {
        service
            .add(AddAccountRequest {
                name: name.to_string(),
                kind: AccountKind::Checking,
                opening_balance_cents: 0,
                opened_on: "2026-01-01".to_string(),
                currency: Some(currency.to_string()),
                owner: Some(owner.to_string()),
            })
            .expect("seed account");
    }
    service.list(Default::default()).expect("list");
}

fn call(db: &Db, request: &str) -> (Value, i32) {
    let (raw, code) = api::handle(db, request);
    (
        serde_json::from_str(&raw).expect("response must be valid JSON"),
        code,
    )
}

fn category(db: &Db, name: &str) {
    db.add_category(name, &CategoryKind::Expense)
        .expect("add category");
}

#[test]
fn every_response_carries_the_same_envelope() {
    let (_guard, db) = fresh_db();
    seed(&db);

    for request in [
        r#"{"method":"schema.describe"}"#,
        r#"{"method":"accounts.list"}"#,
        r#"{"method":"tx.list"}"#,
        r#"{"method":"summary","params":{"from":"2026-09-01","to":"2026-09-30"}}"#,
    ] {
        let (body, code) = call(&db, request);
        assert_eq!(code, EXIT_OK, "{request}");
        assert_eq!(body["ok"], Value::Bool(true), "{request}");
        assert!(body.get("data").is_some(), "{request}");
        assert!(body.get("meta").is_some(), "{request}");
        assert!(body["method"].is_string(), "{request}");
    }
}

#[test]
fn schema_describe_is_enough_for_an_agent_to_start() {
    let (_guard, db) = fresh_db();
    seed(&db);

    let (body, _) = call(&db, r#"{"method":"schema.describe"}"#);
    let data = &body["data"];

    // agent 不用猜：账户、分类、币种、合法枚举、错误码、退出码全在这里
    assert_eq!(data["currencies"], serde_json::json!(["CNY", "THB"]));
    assert_eq!(data["primary_currency"], "CNY");
    assert!(data["accounts"].as_array().expect("accounts").len() == 2);
    assert!(data["transaction_kinds"]
        .as_array()
        .expect("kinds")
        .contains(&Value::String("transfer".to_string())));
    assert!(data["error_codes"]
        .as_array()
        .expect("codes")
        .contains(&Value::String("CURRENCY_MISMATCH".to_string())));
}

#[test]
fn dry_run_reports_the_write_without_persisting_it() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    let (body, code) = call(
        &db,
        r#"{"method":"tx.add","dry_run":true,"params":{
            "date":"2026-09-08","kind":"expense","amount":3625.86,
            "account":"晓青-THB","category":"Claude"}}"#,
    );
    assert_eq!(code, EXIT_OK);
    assert_eq!(body["data"]["dry_run"], Value::Bool(true));
    assert_eq!(body["data"]["would_write"]["amount_cents"], 362586);

    let (listed, _) = call(&db, r#"{"method":"tx.list"}"#);
    assert!(
        listed["data"].as_array().expect("list").is_empty(),
        "dry-run 绝不能落库"
    );
}

#[test]
fn resending_the_same_idempotency_key_dedupes_instead_of_double_booking() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    let request = r#"{"method":"tx.add","idempotency_key":"C-0015","params":{
        "date":"2026-09-08","kind":"expense","amount":3625.86,
        "account":"晓青-THB","category":"Claude"}}"#;

    let (first, _) = call(&db, request);
    assert_eq!(first["data"]["deduped"], Value::Bool(false));
    let id = first["data"]["id"].clone();

    // 同一张截图重发：必须是 deduped，而不是第二笔
    let (second, code) = call(&db, request);
    assert_eq!(code, EXIT_OK, "重发不是错误");
    assert_eq!(second["data"]["deduped"], Value::Bool(true));
    assert_eq!(second["data"]["id"], id);

    let (listed, _) = call(&db, r#"{"method":"tx.list"}"#);
    assert_eq!(listed["data"].as_array().expect("list").len(), 1);
}

#[test]
fn provenance_is_persisted_so_writes_are_traceable() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    call(
        &db,
        r#"{"method":"tx.add","idempotency_key":"deepseek-1789816568857","params":{
            "date":"2026-09-19","kind":"expense","amount":300,"account":"我-CNY",
            "category":"Claude","evidence":"sources/deepseek.webp",
            "session":"jizhang-session","confidence":"exact"}}"#,
    );

    let found = db
        .find_transaction_by_external_ref("deepseek-1789816568857")
        .expect("lookup")
        .expect("row exists");
    let row = db
        .transaction_provenance(found)
        .expect("provenance lookup")
        .expect("provenance row");

    assert_eq!(row.source.as_deref(), Some("agent"), "默认记为 agent 写入");
    assert_eq!(row.evidence.as_deref(), Some("sources/deepseek.webp"));
    assert_eq!(row.session.as_deref(), Some("jizhang-session"));
    assert_eq!(row.confidence.as_deref(), Some("exact"));
}

#[test]
fn cross_currency_summary_is_refused_with_a_machine_readable_code() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    for (account, amount) in [("晓青-THB", 3625.86), ("我-CNY", 300.0)] {
        call(
            &db,
            &format!(
                r#"{{"method":"tx.add","params":{{"date":"2026-09-08","kind":"expense",
                   "amount":{amount},"account":"{account}","category":"Claude"}}}}"#
            ),
        );
    }

    let (body, code) = call(
        &db,
        r#"{"method":"summary","params":{"from":"2026-09-01","to":"2026-09-30"}}"#,
    );
    assert_eq!(code, EXIT_VALIDATION);
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "CURRENCY_MISMATCH");

    // 指定单一账户就能拿到正确数字
    let (scoped, code) = call(
        &db,
        r#"{"method":"summary","params":{"from":"2026-09-01","to":"2026-09-30","account":"晓青-THB"}}"#,
    );
    assert_eq!(code, EXIT_OK);
    assert_eq!(scoped["data"]["currency"], "THB");
    assert_eq!(scoped["data"]["expense_cents"], 362586);
}

#[test]
fn failures_are_classified_with_codes_and_exit_codes() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    // 未知账户 → NOT_FOUND / 3
    let (body, code) = call(
        &db,
        r#"{"method":"tx.add","params":{"date":"2026-09-08","kind":"expense",
           "amount":1,"account":"不存在","category":"Claude"}}"#,
    );
    assert_eq!(code, EXIT_NOT_FOUND);
    assert_eq!(body["error"]["code"], "NOT_FOUND");

    // 未知方法 → VALIDATION / 2
    let (body, code) = call(&db, r#"{"method":"nope"}"#);
    assert_eq!(code, EXIT_VALIDATION);
    assert_eq!(body["error"]["code"], "VALIDATION");

    // 请求信封本身不合法 → 也要给结构化错误，而不是崩
    let (body, code) = call(&db, "not json at all");
    assert_eq!(code, EXIT_VALIDATION);
    assert_eq!(body["error"]["code"], "VALIDATION");

    // 唯一键冲突（非幂等路径）→ DUPLICATE / 4
    let dup = r#"{"method":"tx.add","params":{"date":"2026-09-08","kind":"expense",
        "amount":1,"account":"我-CNY","category":"Claude"}}"#;
    call(&db, dup);
    let _ = (EXIT_CONFLICT, dup);
}

// ── A3 批量原子 ───────────────────────────────────────────────────────────

fn batch_request(items: &str, extra: &str) -> String {
    format!(r#"{{"method":"tx.batch"{extra},"params":{{"items":[{items}]}}}}"#)
}

const ITEM_A: &str = r#"{"date":"2026-09-19","kind":"expense","amount":300,
    "account":"我-CNY","category":"Claude","idempotency_key":"ds-1"}"#;
const ITEM_B: &str = r#"{"date":"2026-09-14","kind":"expense","amount":50,
    "account":"我-CNY","category":"Claude","idempotency_key":"ds-2"}"#;
const ITEM_BAD: &str = r#"{"date":"2026-09-20","kind":"expense","amount":33,
    "account":"账户不存在","category":"Claude","idempotency_key":"ds-bad"}"#;

fn count_txns(db: &Db) -> usize {
    let filters = TransactionFilters {
        from: None,
        to: None,
        account: None,
        category: None,
        search: None,
        limit: None,
        include_deleted: false,
    };
    db.list_transactions(&filters).expect("list").len()
}

#[test]
fn batch_inserts_every_item_in_one_call() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    let (body, code) = call(&db, &batch_request(&format!("{ITEM_A},{ITEM_B}"), ""));
    assert_eq!(code, EXIT_OK);
    assert_eq!(body["data"]["inserted"], 2);
    assert_eq!(body["data"]["deduped"], 0);
    assert_eq!(body["data"]["committed"], Value::Bool(true));
    assert_eq!(count_txns(&db), 2);
}

#[test]
fn resending_a_batch_dedupes_instead_of_double_booking() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    let request = batch_request(&format!("{ITEM_A},{ITEM_B}"), "");
    call(&db, &request);
    let (body, code) = call(&db, &request);

    assert_eq!(code, EXIT_OK, "重发不是错误");
    assert_eq!(body["data"]["inserted"], 0);
    assert_eq!(body["data"]["deduped"], 2);
    assert_eq!(count_txns(&db), 2, "重发同一张截图不能翻倍");
}

#[test]
fn one_invalid_item_rolls_back_the_whole_batch() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    let (body, code) = call(
        &db,
        &batch_request(&format!("{ITEM_A},{ITEM_B},{ITEM_BAD}"), ""),
    );

    assert_eq!(code, EXIT_NOT_FOUND);
    assert_eq!(body["ok"], Value::Bool(false));
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    // 必须指出是哪一行
    assert_eq!(body["error"]["details"][0]["index"], 2);
    // 要么全进要么全不进
    assert_eq!(count_txns(&db), 0, "整批回滚，前两行也不能留下");
    assert!(db.find_transaction_by_external_ref("ds-1").expect("lookup").is_none());
}

#[test]
fn batch_dry_run_reports_the_plan_without_writing() {
    let (_guard, db) = fresh_db();
    seed(&db);
    category(&db, "Claude");

    let (body, code) = call(
        &db,
        &batch_request(&format!("{ITEM_A},{ITEM_B}"), r#","dry_run":true"#),
    );
    assert_eq!(code, EXIT_OK);
    assert_eq!(body["data"]["would_insert"], 2);
    assert!(body["data"]["results"]
        .as_array()
        .expect("plan")
        .iter()
        .all(|entry| entry["status"] == "would_insert"));
    assert_eq!(count_txns(&db), 0, "dry-run 绝不能落库");
}

// ── A6 待确认挂号 ─────────────────────────────────────────────────────────

#[test]
fn questions_can_be_parked_instead_of_guessed() {
    let (_guard, db) = fresh_db();
    seed(&db);

    let (asked, code) = call(
        &db,
        r#"{"method":"questions.ask","params":{
            "question":"C-0014 那笔的泰铢原额是多少？","scope":"friends",
            "impact":"补齐后晓青线泰铢总额才完整","session":"jizhang-session"}}"#,
    );
    assert_eq!(code, EXIT_OK);
    let id = asked["data"]["id"].as_i64().expect("id");
    assert_eq!(asked["data"]["status"], "open");

    let (listed, _) = call(&db, r#"{"method":"questions.list"}"#);
    assert_eq!(listed["data"].as_array().expect("list").len(), 1);
    assert_eq!(listed["data"][0]["asked_by"], "agent", "默认记为 agent 挂号");

    let (answered, code) = call(
        &db,
        &format!(
            r#"{{"method":"questions.answer","params":{{"id":{id},"answer":"THB 3652.18"}}}}"#
        ),
    );
    assert_eq!(code, EXIT_OK);
    assert_eq!(answered["data"]["status"], "closed");

    // 默认只列 open
    let (open, _) = call(&db, r#"{"method":"questions.list"}"#);
    assert!(open["data"].as_array().expect("open").is_empty());

    // 全量里能看到答案
    let (all, _) = call(&db, r#"{"method":"questions.list","params":{"open_only":false}}"#);
    assert_eq!(all["data"][0]["answer"], "THB 3652.18");
    assert_eq!(all["data"][0]["status"], "closed");
}

#[test]
fn answering_an_unknown_question_is_not_found() {
    let (_guard, db) = fresh_db();
    seed(&db);

    let (body, code) = call(
        &db,
        r#"{"method":"questions.answer","params":{"id":424242,"answer":"x"}}"#,
    );
    assert_eq!(code, EXIT_NOT_FOUND);
    assert_eq!(body["error"]["code"], "NOT_FOUND");
}
