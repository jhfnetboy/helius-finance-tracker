# Agent-Native 设计 —— 让记账工具原生服务于 agent

> 定位：**这个工具的第一公民是 agent，第二公民是人。**
> 人可以走网页界面；agent 必须能原生、结构化、可幂等、可自省地调用它。
> 状态：设计已定，实施中。

---

## 0. 为什么"加个 `--json`"不算 agent-native

上游 Helius 有 `--json`，但它仍然是为人类设计的 CLI：输出是给人看的表格、
错误是散文、重复执行会产生重复记录、agent 无法自省"库里到底有哪些账户和币种"。

对 agent 来说，真正缺的是**契约**，不是格式。下面七条才是 agent-native 的判据。

## 1. 七条 agent-native 契约

| # | 契约 | 为什么记账场景必需 |
|:---|:---|:---|
| **A1** | **结构化 I/O，单次进程** | 无 TTY、无颜色、无交互提示、无分页。一次调用一个动作，stdout 只有 JSON。 |
| **A2** | **幂等** | agent 会重发。你重发同一张 Billing 截图时，**绝不能重复记账**。靠 `external_ref` 唯一键 + upsert 语义。 |
| **A3** | **批量原子** | 一张截图 9 行。要么全进，要么全不进；返回逐行结果（成功/去重/失败）。 |
| **A4** | **dry-run** | 先看后写。配合你定的规矩「每笔先确认归属」——agent 先产出**计划**给人过目。 |
| **A5** | **溯源** | 每笔记录：谁写的（agent/human）、依据什么（凭证文件）、来自哪个会话、置信度。财务数据必须可回溯。 |
| **A6** | **待确认是一等公民** | 归属判不准时，agent 要能**挂号**而不是猜。`questions.ask/list/answer`。 |
| **A7** | **自描述** | `schema.describe` 一次性返回全部账户/分类/币种/字段约束，agent 不用猜也不用先 `--help` 试错。 |

## 2. 契约细节

### 请求信封

```json
{
  "method": "tx.add",
  "params": { "...": "..." },
  "dry_run": false,
  "idempotency_key": "deepseek-1789816568857jlJUCuAwEcvlZJbc"
}
```

### 响应信封（永远是这个形状，读/写一致）

```json
// 成功
{ "ok": true, "method": "tx.add", "data": { "id": "C-0018", "deduped": false },
  "meta": { "schema_version": 11, "currency": "THB" } }

// 失败 —— 带机器可读的错误码，agent 据此自我修正
{ "ok": false, "method": "tx.add",
  "error": { "code": "UNKNOWN_ACCOUNT", "message": "...", "hint": "先调 schema.describe" } }
```

**错误码表**（agent 靠 code 分支，不靠解析 message）：

| code | 含义 | agent 该怎么做 |
|:---|:---|:---|
| `VALIDATION` | 参数不合法 | 修正参数重试 |
| `UNKNOWN_ACCOUNT` / `UNKNOWN_CATEGORY` | 引用不存在 | 调 `schema.describe` 拿合法值 |
| `CURRENCY_MISMATCH` | 跨币种相加/混账 | 按币种拆开，或指定账户 |
| `DUPLICATE` | 唯一键冲突（非幂等键） | 改用 `idempotency_key` 或改内容 |
| `NOT_FOUND` | 目标不存在 | 停止，问人 |
| `CONFLICT` | 状态冲突（如账户已归档） | 停止，问人 |

### 退出码

`0` 成功 ｜ `2` 参数/校验错 ｜ `3` 找不到 ｜ `4` 冲突 ｜ `5` 内部错误

### 幂等语义

- `idempotency_key` 落在 `transactions.external_ref`（UNIQUE）。
- 同 key 重复提交 → `ok:true` + `deduped:true` + 原记录 id，**不报错**（重发是正常行为）。
- 内容与已存在记录不同（如金额改了）→ `DUPLICATE` + 提示原记录，**不静默覆盖财务数据**。

### 溯源字段（schema v11 新增）

| 列 | 取值 |
|:---|:---|
| `source` | `agent` / `human` / `import` |
| `evidence` | 凭证路径（如 `cost/sources/2026-09-19-deepseek-billing.webp`）或 `口述` |
| `session` | 写入方会话标识，便于回溯"这是哪次对话记的" |
| `external_ref` | 幂等键 / 原始单号 |
| `confidence` | `exact`（凭证明确）/ `derived`（换算/推断）/ `uncertain`（待确认） |

## 3. 三种接入形态（共用同一个方法层）

```
                    ┌───────────────────────────┐
                    │   Method Layer（唯一实现） │
                    │  accounts.* / tx.* /       │
                    │  recurring.* / summary /   │
                    │  questions.* / schema.*    │
                    └───────────┬───────────────┘
        ┌───────────────────────┼───────────────────────┐
        │                       │                       │
  helius api              helius mcp              helius serve
  （JSON 单次调用）        （MCP stdio server）      （人类网页界面）
  任何 agent 都能用        原生 tool 调用           浏览器看板
  经 bash / exec           DSH / Claude Code 等     只读 + 人工确认
```

- **`helius api`**：最通用。任何能执行进程的 agent 都能用，不依赖 MCP 生态。
- **`helius mcp`**：把同一批方法暴露成 MCP tools，供支持的 agent 原生调用。
- **`helius serve`**：人类界面，且是 dry-run 计划的**确认入口**。

## 4. 信任模型

| 操作 | 策略 |
|:---|:---|
| 读（`*.list` / `summary` / `schema.describe`） | 放开，agent 随便调 |
| 写（`tx.add` / `recurring.add`） | 允许，但必须带 `source` 与 `evidence`，且支持 `dry_run` |
| 改 / 删（edit / delete / archive） | 必须显式 `"confirm": true`；默认 `dry_run` 返回影响面 |
| 跨币种求和 | **永远拒绝**（`CURRENCY_MISMATCH`），不给错数 |
| 网络 | 只监听 `127.0.0.1`；工具本身不发起任何外部请求 |

## 5. 实施顺序

| 阶段 | 内容 | 状态 |
|:---|:---|:---|
| **N1** | schema v11：`txn` 溯源列（`source`/`evidence`/`session`/`external_ref`/`confidence`）+ 唯一索引 | ✅ |
| **N2** | `helius api`：信封 + 错误码 + 退出码 + 方法分发；`schema.describe`、`accounts.list`、`tx.list`、`summary`、`tx.add`（幂等 + dry_run + 溯源） | ✅ |
| **N3** | `tx.batch`（A3 批量原子）与 `questions.*`（A6 挂号） | ✅ |
| **N4** | `helius mcp`（MCP stdio server，薄适配层） | ⬜ |
| **N5** | `helius serve` 网页界面 + dry-run 确认入口 | ⬜ |
| **N6** | 迁移 `cost/` 现有数据（用幂等键，天然可重跑） | ⬜ |

### A3 批量原子的语义

```json
{"method":"tx.batch","params":{"items":[ {…}, {…}, {…} ]}}
```

- **先全部解析引用**（此时不写库）→ 全部通过才开事务逐笔插入
- 任何一笔失败 → **ROLLBACK 整批**，并在 `error.details[index]` 指出是**哪一行**
- 已存在幂等键的行计为 `deduped`，不算失败
- 响应：`{total, inserted, deduped, failed, committed, results[]}`

```
① 3 行一次提交   → inserted:3, committed:true
② 重发同一批     → deduped:3, inserted:0，库内仍是 3 笔
③ 第 3 行账户写错 → NOT_FOUND + details[{index:2}]，**前两行也被回滚**（good-1 未落库）
```

### A6 待确认（挂号处）

| 方法 | 参数 | 说明 |
|:---|:---|:---|
| `questions.ask` | `question`（必填）、`scope`、`impact`、`session` | 归属判不准时挂号，**不猜**；返回 `{id, status:"open"}` |
| `questions.list` | `open_only`（默认 true） | 列出待确认 |
| `questions.answer` | `id`、`answer` | 回答并关闭 |

### 已可用（N2）实测

```bash
H=cost/tools/helius/target/release/helius

echo '{"method":"schema.describe"}' | $H --db $DB api
echo '{"method":"tx.add","dry_run":true,"params":{"date":"2026-09-08","kind":"expense",
      "amount":3625.86,"account":"晓青-THB","category":"Claude"}}' | $H --db $DB api
echo '{"method":"tx.add","idempotency_key":"C-0015","params":{...}}' | $H --db $DB api
  # 重发同一幂等键 → {"deduped":true,"id":1}，不会重复记账
```

错误码实测：`NOT_FOUND`(exit 3) / `VALIDATION`(2) / `CURRENCY_MISMATCH`(2)
| **N4** | `helius mcp`（MCP stdio server，薄适配层） |
| **N5** | `helius serve` 网页界面 + dry-run 确认入口 |
| **N6** | 迁移 `cost/` 现有数据（用幂等键，天然可重跑） |

## 6. 给 agent 的一句话上手说明

> 调 `helius api`，读 stdin 拿请求、写 stdout 给响应；先 `schema.describe` 摸清合法值，
> 写之前 `dry_run:true` 过一遍；重发同一张截图用相同 `idempotency_key`，不会重复记账。
