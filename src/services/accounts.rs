use crate::db::Db;
use crate::error::AppError;
use crate::model::{Account, AccountKind};

#[derive(Clone, Debug)]
pub struct AddAccountRequest {
    pub name: String,
    pub kind: AccountKind,
    pub opening_balance_cents: i64,
    pub opened_on: String,
    /// 账户币种。`None` = 沿用主币种。
    pub currency: Option<String>,
    /// 归属人。`None` = 未指定。
    pub owner: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct EditAccountRequest {
    pub reference: String,
    pub name: Option<String>,
    pub kind: Option<AccountKind>,
    pub opening_balance_cents: Option<i64>,
    pub opened_on: Option<String>,
    /// `None` = 不改动币种。
    pub currency: Option<String>,
    /// `None` = 不改动归属人。
    pub owner: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DeleteAccountRequest {
    pub reference: String,
}

#[derive(Clone, Debug, Default)]
pub struct ListAccountsRequest;

pub struct AccountService<'a> {
    db: &'a Db,
}

impl<'a> AccountService<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn add(&self, req: AddAccountRequest) -> Result<i64, AppError> {
        self.db.add_account(
            &req.name,
            &req.kind,
            req.opening_balance_cents,
            &req.opened_on,
            req.currency.as_deref(),
            req.owner.as_deref(),
        )
    }

    pub fn edit(&self, req: EditAccountRequest) -> Result<i64, AppError> {
        if req.name.is_none()
            && req.kind.is_none()
            && req.opening_balance_cents.is_none()
            && req.opened_on.is_none()
            && req.currency.is_none()
            && req.owner.is_none()
        {
            return Err(AppError::Validation(
                "account edit requires at least one field change".to_string(),
            ));
        }
        self.db.edit_account(
            &req.reference,
            req.name.as_deref(),
            req.kind.as_ref(),
            req.opening_balance_cents,
            req.opened_on.as_deref(),
            req.currency.as_deref(),
            req.owner.as_deref(),
        )
    }

    pub fn delete(&self, req: DeleteAccountRequest) -> Result<i64, AppError> {
        self.db.delete_account(&req.reference)
    }

    pub fn list(&self, _req: ListAccountsRequest) -> Result<Vec<Account>, AppError> {
        self.db.list_accounts()
    }
}
