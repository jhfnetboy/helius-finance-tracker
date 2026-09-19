# Fork 改动记录 —— Helius (多币种记账)

> 上游：[Helius-Finance/helius-finance-tracker](https://github.com/Helius-Finance/helius-finance-tracker) v1.4.3（203★，AGPL-3.0-only）
> 本地路径：`cost/tools/helius/`（在 `.gitignore` 的 `cost/` 内，不会污染 Brood 仓库）
> 为什么选它：Rust + `rusqlite` 的 `bundled` feature → **真·单二进制自带 SQLite**；CLI + TUI；`recurring_rules` 正好是"每月几号扣多少"。

## 仓库位置与同步

| remote | 仓库 | 说明 |
|:---|:---|:---|
| `origin` | https://github.com/jhfnetboy/helius-finance-tracker | **我们自己的 fork**（public，`main` 跟踪它） |
| `upstream` | https://github.com/Helius-Finance/helius-finance-tracker | 官方上游 |

```bash
# 跟进官方更新
git fetch upstream && git merge upstream/main
git push origin main
```

> ⚠️ 这是一个**公开**仓库。账本数据库（`*.db` / `*.sqlite*`）已被上游 `.gitignore` 排除，
> 个人流水**不会**进入 git —— 但每次 `git add` 前仍建议 `git status` 扫一眼。
> 顺便：上游是 **AGPL-3.0-only**，自己用没问题；对外分发/对外提供服务则必须开源。

## 编译方式（macOS）

沙箱不允许写 `~/.cargo`，所以把 CARGO_HOME 放进工作区：

```bash
cd cost/tools/helius
export CARGO_HOME=/Users/jason/Dev/Brood/cost/tools/.cargo
cargo build --release          # → target/release/helius（3.8MB 单二进制）
cargo test  --release          # → 123 passed, 0 failed
```

上游 CI 只发 Windows/Linux 包，但**源码在 macOS 上原样通过编译**，无需打补丁。

## 改动 1：多币种 + 归属人（schema v9 → v10）✅ 已完成

### 上游的问题

`metadata.currency` 是**整库唯一**币种，`accounts` 没有币种列。实测证据：

```
两笔支出：THB 3625.86（晓青）+ ¥300（我）
→ summary month 2026-09  ⇒  Expense -3925.86      ❌ 两个币种被直接相加
```

### 改法

新增两个**可空**列（`NULL` = 沿用主币种 / 无归属人，所以旧库零回填、语义不变）：

```sql
ALTER TABLE accounts ADD COLUMN currency TEXT;   -- THB / USD / CNY / EUR
ALTER TABLE accounts ADD COLUMN owner    TEXT;   -- 我 / 晓青 / F哥
```

**建模约定：一个「人 × 一种币种」= 一个账户。** 例如 `晓青-THB`、`F哥-USD`、`我-CNY`。
这样跨币种加法在结构上就不会发生。

### 改动清单（221 行，11 文件）

| 文件 | 改了什么 |
|:---|:---|
| `src/db.rs` | schema v10、`migrate_v9_to_v10`、`add/edit/list/load_account` 带币种、`normalize_optional_currency` / `normalize_owner`、`summary_currencies`、`account_effective_currency`、**`summary_all_accounts` fail-closed** |
| `src/model.rs` | `Account.currency/owner`、`SummaryRecord.currency` |
| `src/services/accounts.rs` | Add/Edit 请求带币种与归属人 |
| `src/cli.rs` | `account add/edit --currency --owner` |
| `src/lib.rs` | 分发接线；`pub use CURRENT_SCHEMA_VERSION` |
| `src/output.rs` | 账户列表加 Currency/Owner 列；summary 表加 Currency 行 |
| `src/ui.rs` `src/ui/app.rs` | TUI 暂不收集这两字段（传 `None` = 保留/沿用，**不会误清空**） |
| `tests/*` | 补字段；schema 断言改为跟随 `CURRENT_SCHEMA_VERSION` |

### 验证结果

```
① 跨币种汇总  → 报错拒绝（不再给错数）✅
   "this range spans 2 currencies (CNY, THB) and they must not be summed together;
    narrow it down with `--account <name>`"
② summary --account 晓青-THB  → Currency: THB, Expense -3625.86   ✅
③ summary --account 我-CNY    → Currency: CNY, Expense -300.00    ✅
④ v9 → v10 迁移：降级旧库再打开 → 自动加列 + 版本升到 10          ✅
⑤ cargo test --release        → 123 passed / 0 failed             ✅
```

## 改动 2：agent-native（schema v11 + `helius api`）✅ 已完成

完整设计见 [`AGENT-NATIVE.md`](AGENT-NATIVE.md)。要点：**这个工具的第一公民是 agent。**

上游的 `--json` 只是给人看的格式，缺的是**契约**。已实现七条中的 A1/A2/A4/A5/A7：

| 契约 | 实现 |
|:---|:---|
| A1 结构化 I/O | `helius api`：stdin 收请求信封，stdout 出响应信封；无 TTY/无颜色/无交互 |
| A2 幂等 | `idempotency_key` → `transactions.external_ref` UNIQUE；重发返回 `deduped:true`，**不重复记账** |
| A4 dry-run | `"dry_run":true` 返回 `would_write`，**不落库**（实测流水数仍为 0） |
| A5 溯源 | 每笔记 `source`/`evidence`/`session`/`confidence`，财务数据可回溯到"哪次对话、哪张截图" |
| A7 自省 | `schema.describe` 一次返回账户/分类/币种/合法枚举/错误码/退出码，agent 不用猜 |
| — 错误码 | `VALIDATION`/`NOT_FOUND`/`DUPLICATE`/`CURRENCY_MISMATCH`/… + 退出码 0/2/3/4/5 |

**测试：130 passed / 0 failed**（其中 `tests/api.rs` 7 个用例专门固定这套契约）。

## 待办路线图

| 优先级 | 事项 | 说明 |
|:---|:---|:---|
| **N3** | `tx.batch` + `questions.*` | 批量原子（一张截图 9 行）+ 归属判不准时挂号而非猜 |
| **N4** | `helius mcp` | MCP stdio server，让支持的 agent 原生 tool 调用（薄适配层，复用 api 方法层） |
| **P1** | 报表按人分组 | `owner` 列已在库里，还差按 owner 聚合 —— 「晓青这条线垫了多少、收回多少」 |
| **N5** | `helius serve` 网页界面 | 人类看板 + dry-run 计划的确认入口。做进 Rust 二进制（单程序），只听 127.0.0.1 |
| **N6** | 数据迁移 | 把 `cost/` 现有 17 笔流水 + 5 个订阅灌进 Helius 库（用幂等键，天然可重跑） |
| **收尾** | 停用我自建的 `cost/db/`（Node + 自写 schema） | 避免两套真相。迁移完成后它只留作历史参考 |

## 使用方式

```bash
H=cost/tools/helius/target/release/helius
DB=cost/helius.db            # 建议位置（也可用 HELIUS_DB_PATH 环境变量）

$H --db $DB init --currency CNY
$H --db $DB account add "我-CNY"   --type cash     --currency CNY --owner 我
$H --db $DB account add "晓青-THB" --type checking --currency THB --owner 晓青
$H --db $DB category add Claude --kind expense
$H --db $DB tx add --type expense --amount 3625.86 --date 2026-09-08 \
      --account "晓青-THB" --category Claude --note "C-0015"
$H --db $DB recurring add "晓青-Claude" --type expense --amount 111 \
      --account "晓青-THB" --category Claude --cadence monthly --day-of-month 4 --start-on 2026-07-05
$H --db $DB summary month 2026-09 --account "晓青-THB"
$H --db $DB                       # 无子命令 → 全屏 TUI
```

## 注意

- **AGPL-3.0-only**：自己用/改没问题；一旦对外分发（含把改过的版本放到网上提供服务），必须开源。个人本地使用无此义务。
- 金额一律以**整数"分"**存储（`amount_cents`），这是上游的既有设计，对我们有利（不丢精度）。
