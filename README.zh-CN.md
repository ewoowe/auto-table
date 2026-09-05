# auto-table

[English](README.md) | [中文文档](README.zh-CN.md)

基于 [SeaORM](https://crates.io/crates/sea-orm) 的建表与迁移工具库。通过属性宏在编译期收集所有实体，在应用启动时自动创建缺失的表，并可将已存在的表结构与实体定义对齐（支持 MySQL 与 SQLite）。

## 组成

本 workspace 包含两个 crate：

| Crate | 说明 |
| --- | --- |
| [`auto-table-core`](auto-table-core) | 核心库，提供 `#[auto_table]` / `#[auto_create]` 宏的运行时支持、表结构读取与比对、迁移计划与执行 |
| [`auto-table-derive`](auto-table-derive) | 过程宏实现（proc-macro） |

> 通常你只需依赖 `auto-table-core`，它已通过 `pub use` 重新导出了两个过程宏。

## 使用

在 `Cargo.toml` 中添加依赖：

```toml
[dependencies]
auto-table-core = "0.3.0"
```

### 1. 标记实体

在 SeaORM Entity 的 `Model` 结构体上使用 `#[auto_table]`：

```rust
use sea_orm::entity::prelude::*;

#[auto_table]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
```

### 2. 注入建表逻辑

在数据库初始化函数上使用 `#[auto_create(db)]`，其中 `db` 是函数体内 `DatabaseConnection` 变量的名称：

```rust
#[auto_create(db)]
pub async fn init_pool(database_url: &str) -> anyhow::Result<()> {
    let db = Database::connect(database_url).await?;
    db.ping().await?;
    DB.set(db).expect("...");
    Ok(())
}
```

宏会定位函数体中的 `let db = ...` 绑定语句，并把建表逻辑注入到它的正后方，这样建表在连接建立后、`db` 被 move 之前执行，且不依赖函数体末尾有多少条语句。

注入的建表逻辑不会打印任何日志，而是由生成的建表函数返回 [`auto_table_core::TableCreationReport`](auto-table-core/src/lib.rs)，其中包含 `existing_tables`（已存在、被跳过的表）与 `created_tables`（本次新建的表）。注入的语句会把该报告绑定到名为 `__auto_table_report` 的局部变量，该变量在注入点之后的整个函数体中可用，可在其中记录日志或做后续处理。变量名以下划线开头，不使用也不会触发未使用警告。若需在宏之外获取报告，也可直接调用 [`auto_table_core::create_missing_tables`](auto-table-core/src/lib.rs)。

### 3. 后端特性开关

默认只启用 MySQL 驱动。可按需切换或组合（`default = ["mysql"]`）：

```toml
# 仅 PostgreSQL
auto-table-core = { version = "0.3.0", default-features = false, features = ["postgres"] }

# 同时支持 MySQL 与 SQLite
auto-table-core = { version = "0.3.0", default-features = false, features = ["mysql", "sqlite"] }
```

可用特性：`mysql`、`postgres`、`sqlite`。三者是叠加的，可同时启用；**至少需启用一个**，否则编译期直接报错。

> 特性只决定编译进哪些**数据库驱动**。后端在运行时由 `DbBackend` 判别，因此查询已有表等逻辑对三个后端都会被编译进来；但要真正连上某个后端，仍需启用对应的驱动。

### 4. 迁移已存在的表

`#[auto_create]` 只会创建缺失的表。已经存在的表若结构与实体定义不一致，可用迁移对齐：

```rust
// 生成计划，此时尚未执行任何语句（即 dry-run）
let plan = auto_table_core::plan_migrations(&db).await?;
for sql in plan.statements() {
    println!("{sql}");
}

// 确认无误后再执行
auto_table_core::apply_migrations(&db, &plan).await?;
```

迁移是**声明式**的：每次都拿实体定义与数据库当前结构做 diff，而非按版本号依次执行。因此它是幂等的——执行完再生成一次计划必然为空；中途失败也可在修复数据后重跑，已完成的变更不会重复。

MySQL、SQLite 与 PostgreSQL 三个后端均支持迁移。三者的语句生成方式不同，主要差异见下文各节。

### 5. 并发安全（多实例部署）

多个实例同时启动时若都去迁移，后到的会重复执行已被应用的语句而失败。加锁可以避免：

```rust
use auto_table_core::{migrate, MigrateOptions};

// 拿不到锁就等待，超时则报错
migrate(&db, MigrateOptions::locked(10)).await?;

// 拿不到锁就跳过——反正另一个实例正在应用同样的变更
migrate(&db, MigrateOptions::skip_if_locked(0)).await?;
```

默认的 `apply_migrations` **不加锁**，与先前行为一致；单实例部署无需关心。

**加锁时请用 `migrate`，而不是先 `plan_migrations` 再 `apply_migrations_with`**。`migrate` 在**拿到锁之后**才生成计划，因此只规划一次、且计划不会过期；先规划再执行的话，为了保证安全仍需在锁内重新规划一次，等于做了两遍。`plan_migrations` 仍应单独使用的场景只有一种——你想在执行前先看看语句是什么（即 dry-run）。

两点实现说明：

- **MySQL** 使用命名锁 `GET_LOCK`。它是**会话级**的，因此整个迁移在单个事务内执行以固定连接，否则锁形同虚设。锁名附带数据库名，同一实例上的不同库互不阻塞。
- **PostgreSQL** 使用 `pg_advisory_lock`（在一个固定 key 上取锁）。它同样是会话级的，因此迁移也在单个事务内执行以保住锁；非阻塞的 `skip` 模式使用 `pg_try_advisory_lock`。
- **SQLite** 没有命名锁，依赖其自身的写锁；加锁只是额外设置一个 `busy_timeout`，让并发实例排队而不是立刻收到 `SQLITE_BUSY`。

无论哪个后端，**拿到锁之后都会重新生成一次计划**：等待锁的这段时间里另一个实例可能已经迁移完毕，重放过期计划只会失败。

若需要更细粒度的控制，也可以直接使用这些构件自行组装：

- [`get_table_schema`](auto-table-core/src/schema.rs) —— 读取某张表当前的结构
- [`parse_create_table`](auto-table-core/src/parse.rs) —— 把实体生成的 `CREATE TABLE` 解析成同样的结构
- [`diff_table`](auto-table-core/src/diff.rs) —— 比对两份结构，得到变更清单
- [`plan_table_migration`](auto-table-core/src/migrate.rs) —— 把单张表的变更清单变成语句（MySQL 与 SQLite 均适用）

#### 迁移对已有数据的影响

下表为 MySQL 8 在**严格模式**（默认开启，含 `STRICT_TRANS_TABLES`）下的实测行为：

| 变更 | 已有数据 | 结果 |
| --- | --- | --- |
| 新增列（可空） | 填 `NULL` | 安全 |
| 新增列（`NOT NULL` 且无默认值） | 填空串 `''` 或 `0` | 静默填充，建议显式指定 `default_value` |
| 加宽类型（`int` → `bigint`） | 原值保留 | 安全 |
| 收窄类型（存在超出范围的值） | — | 报错，整条语句失败 |
| 收紧为 `NOT NULL`（存在 `NULL`） | — | 报错，整条语句失败 |
| 新增唯一索引（存在重复值） | — | 报错，整条语句失败 |
| 修改默认值 | **不影响已有行** | 安全 |
| 删除索引 | 不影响数据 | 安全 |
| **删除列** | **整列数据永久丢失** | **成功且不可逆** |

两点需要特别留意：

- **删除列不可逆**。MySQL 的 DDL 不在事务中回滚，一旦执行就无法撤销。
- **关闭严格模式会改变上表结果**：收窄类型不再报错，而是静默截断（如 `3000000000` 变为 `2147483647`），数据会被悄悄损坏。

#### SQLite 的迁移场景

SQLite 既没有 `MODIFY COLUMN` 也没有 `ALTER COLUMN`，**列定义一旦建立就无法修改**。因此迁移分为两条路：

- 增删列 —— 用普通的 `ALTER TABLE`；
- 列定义的任何变更（类型、是否可空、默认值）以及索引与约束的变更 —— **重建表**：建新表 → 拷贝数据 → 删旧表 → 改名 → 重建索引。

各场景对已有数据的影响（SQLite 3.45 实测）：

| 变更 | 做法 | 已有数据 |
| --- | --- | --- |
| 新增列（可空） | `ADD COLUMN` | 填 `NULL` |
| 新增列（`NOT NULL` 有默认值） | `ADD COLUMN` | 填默认值 |
| 新增列（`NOT NULL` 无默认值） | — | **直接报错**，不执行 |
| 删除列 | `DROP COLUMN`（需 3.35+） | 整列数据永久丢失 |
| 类型变更（同一亲和族，如 `int` → `bigint`） | 不产生语句 | 无影响，存储行为本就相同 |
| 类型变更（跨亲和族） | 重建表 | 按新亲和性转换；无法转换的值原样保留 |
| 收紧为 `NOT NULL` | 重建表 | 存在 `NULL` 时失败并回滚，**原表数据完好** |
| 放宽为可空 | 重建表 | 无影响 |
| 修改默认值 | 重建表 | **已有行保持原值**，只影响此后新插入的行 |
| 新增唯一约束 | 重建表 | 存在重复值时失败并回滚，**原表数据完好** |
| 删除索引 | 重建表 | 数据不受影响 |

两点与 MySQL 的明显反差：

- **新增必填列时 SQLite 更严格**：`NOT NULL` 且没有默认值时它直接报错；而 MySQL 会静默把已有行填成 `''` 或 `0`。
- **重建表失败会整体回滚**：MySQL 的 DDL 不在事务中回滚，SQLite 则可以，所以重建途中出错时表会恢复到迁移前的状态。

需要注意的是，重建表会重写整张表，在大表上代价较高。

#### PostgreSQL 的迁移场景

PostgreSQL 把列变更拆成**每个方面一条语句**，不像 MySQL 用一个 `MODIFY COLUMN` 囊括：

| 变更 | 语句 |
| --- | --- |
| 新增列 | `ALTER TABLE t ADD COLUMN "c" 类型 [NOT NULL] [DEFAULT x]` |
| 删除列 | `ALTER TABLE t DROP COLUMN "c"` |
| 改变类型 | `ALTER TABLE t ALTER COLUMN "c" TYPE 新类型` |
| 收紧为 `NOT NULL` | `ALTER TABLE t ALTER COLUMN "c" SET NOT NULL` |
| 放宽为可空 | `ALTER TABLE t ALTER COLUMN "c" DROP NOT NULL` |
| 设置默认值 | `ALTER TABLE t ALTER COLUMN "c" SET DEFAULT x` |
| 去除默认值 | `ALTER TABLE t ALTER COLUMN "c" DROP DEFAULT` |
| 新增唯一约束 | `ALTER TABLE t ADD CONSTRAINT "t_c_key" UNIQUE ("c")` |
| 删除唯一约束 | `ALTER TABLE t DROP CONSTRAINT "t_c_key"` |

所以一次列定义的变更可能产出多条语句，例如同时改类型与可空性会生成两条。

另有两点需要处理：

- **约束名遵循 PostgreSQL 的规则**：主键是 `<表>_pkey`，唯一约束是 `<表>_<列>_key`。读取结构时先换算成逻辑名再比对，生成语句时才还原为物理名——否则两者永远对不上。
- **类型名做了归一**：PostgreSQL 把 `varchar` 报作 `character varying`、`decimal` 存作 `numeric`、`bool` 称作 `boolean`。不归一的话，字符串、小数与布尔列每次都会被误判为差异。

#### 危险操作分级

两种"危险"性质不同，防护方式也不该相同：

- **Caution（可能失败）**：迁移被拒绝——MySQL 在严格模式下整条语句失败，SQLite 则是重建失败并整体回滚，**两者的数据都原封不动**。它们不会毁数据，只会让迁移中断。
- **Destructive（可能毁数据）**：无条件成功、**数据永久丢失且无法撤销**，两个后端皆是如此。

只有后者需要人工授权，前者只需要提示。分级如下，三后端均为实测结果：

| 变更 | 级别 | MySQL | SQLite | PostgreSQL |
| --- | --- | --- | --- | --- |
| 新增可空列 | Safe | 填 `NULL` | 填 `NULL` | 填 `NULL` |
| 新增 `NOT NULL` 列（有默认值） | Safe | 填默认值 | 填默认值 | 填默认值 |
| 加宽类型（如 `int` → `bigint`） | Safe | 无损 | 不产生语句，亲和性相同 | 无损 |
| 修改默认值 | Safe | 不影响已有行 | 重建表，已有行保持原值 | 不影响已有行 |
| 删除索引 | Safe | 不影响数据 | 重建表，数据不受影响 | 不影响数据 |
| 新增 `NOT NULL` 列（无默认值） | Caution | **静默填 `''` 或 `0`** | 直接报错 | **除非表为空，否则报错** |
| 收窄类型 | Caution | 严格模式下报错 | 按亲和性转换，无法转换的值原样保留 | 值放不下时报错 |
| 收紧为 `NOT NULL` | Caution | 存在 `NULL` 时报错 | 重建失败并回滚，原表完好 | 存在 `NULL` 时报错 |
| 新增唯一索引/约束 | Caution | 存在重复值时报错 | 重建失败并回滚，原表完好 | 存在重复值时报错 |
| **删除列** | **Destructive** | **数据永久丢失，不可回滚** | **数据永久丢失，不可回滚** | **数据永久丢失** |

API：

```rust
// 每个变更可自报风险等级与具体类型
pub enum Risk { Safe, Caution, Destructive }
pub enum ChangeKind {
    AddColumn, AddNotNullColumn, DropColumn, ChangeType,
    TightenNullability, RelaxNullability, ChangeDefault,
    ChangeAutoIncrement, AddIndex, DropIndex,
}

// 默认：发现破坏性变更就拒绝执行任何语句
apply_migrations(&db, &plan).await?;

// 显式授权后才执行（等价于把 Destructive 这一风险等级设为允许）
apply_migrations(&db, &plan.allow_destructive()).await?;

// 三层配置开关：global（全局）→ levels（按风险等级）→ items（按具体变更类型）
// 越具体的层优先级越高；未设置的层向下回退，global 始终生效。
use auto_table_core::{RiskPolicy, RiskAction, ChangeKind, Risk};
let mut policy = RiskPolicy::default();
policy.global = RiskAction::Block;                            // L1：默认全部拒绝
policy.levels.insert(Risk::Caution, RiskAction::Allow);       // L2：Caution 放行
policy.items.insert(ChangeKind::DropColumn, RiskAction::Block); // L3：删列即便被 L2 放行也仍拒绝
apply_migrations_with(&db, &plan, MigrateOptions::default().with_risk_policy(policy)).await?;
```

三层开关的生效优先级为 **L3（具体类型）> L2（风险等级）> L1（全局）**。`allow_destructive()` 是"放行 Destructive 等级"的简写，因此仍会被 L3 上对该具体变更类型的显式 `Block` 覆盖。默认 `RiskPolicy` 仅拦截 Destructive（删列），其余照常执行。

两处设计取舍：

- **先检查、后执行**：在跑任何语句之前先扫描整个计划，发现未授权的破坏性变更就立即报错、一条都不执行。MySQL 的 DDL 不能回滚，"执行到一半才发现危险"会把数据库留在半成品状态；对 SQLite 而言也能省下一次白白的大表重建。
- **破坏性只判删除列**：宁可标准单一也要可信。若把类型收窄也算破坏性，几乎每次改字段都要授权，用户很快养成习惯性确认，防护便形同虚设。

> PostgreSQL 现已完整支持并实测。一个值得注意的分歧：「新增 `NOT NULL` 列但无默认值」在 PostgreSQL 与 SQLite 中都会**报错**（除非表为空），而 MySQL 会静默把已有行填成 `''` 或 `0`。

> 已实现：只要计划中包含删列，默认 `apply_migrations` 就会拒绝执行，并返回 `TableError::DestructiveChangesBlocked`；只有 `allow_destructive` 显式授权才会真正执行。端到端测试 `drops_a_column_*` 正是覆盖这一行为——默认应用被拒，授权后才成功并收敛。

> 除上述整体授权外，还提供三层 `RiskPolicy` 开关（全局 / 风险等级 / 具体变更类型），可对任意风险项做 `Allow` / `Block` 的细粒度控制，越具体的层优先级越高。端到端测试 `policy_*` 覆盖其优先级行为。

## 支持的后端

自动建表与迁移均支持以下三者。

- MySQL
- PostgreSQL
- SQLite

## 错误处理

核心库通过 `thiserror` 暴露精确的 [`auto_table_core::TableError`](auto-table-core/src/lib.rs)，上层应用可经 `?` 自动装箱为 `anyhow::Error` 传播。

## 路线图

- [x] **数据库迁移（migration）** — 三个后端均可用（见「4. 迁移已存在的表」与「PostgreSQL 的迁移场景」），并发安全已通过各后端的原生锁实现：MySQL `GET_LOCK`、SQLite 写锁 + `busy_timeout`、PostgreSQL `pg_advisory_lock`（见「5. 并发安全」）；拿到锁后会重新生成计划，绝不重放过期计划。
  - 危险操作分级（见「危险操作分级」设计草案）尚未强制：删除列仍是破坏性操作，要等拟定的授权 API 落地后才有保护。
- [x] **SQLite 迁移（含重建表）** — SQLite 不支持 `MODIFY COLUMN`，故列定义变更与索引/约束变更都走「建新表 → 拷贝数据 → 删旧表 → 重命名」流程，在单个事务内完成、失败自动回滚（详见「SQLite 的迁移场景」）
- [ ] 迁移回滚 — 本库迁移是声明式的（每次对比当前状态与目标状态），难以自动生成 down：`DROP COLUMN` 后数据已丢失，且 MySQL 的 DDL 不支持在事务中回滚（SQLite 则可以，已实测）。计划先落地「失败时按逆操作尽力回滚」，有损步骤明确报错而非静默继续
- [x] 更细粒度的后端特性开关（按需启用 MySQL / PostgreSQL / SQLite）

## 测试覆盖

本库的测试分两层，覆盖三个后端上的每一种迁移场景：

- **单元测试** 紧挨计划器放在 `auto-table-core/src/backend/{mysql,postgres,sqlite}.rs`。它们逐一断言每个 `IndexChange` / `ColumnAspect` 生成的确切 DDL，以及类型归一与语句排序——无需数据库。（72 个测试）
- **端到端测试** 在 `auto-table-core/tests/{mysql,pg,sqlite}_e2e.rs`，跑通完整链路——读取线上结构 → 与实体做 diff → 生成计划 → 执行真实 DDL → 再计划一次应得到空计划。它们分别对接真实的 MySQL 8、PostgreSQL 18 与内置 SQLite。（19 + 15 + 11 = 45 个测试）

设置好 `AUTO_TABLE_TEST_DATABASE_URL` / `AUTO_TABLE_TEST_POSTGRES_URL` 后执行 `cargo test -p auto-table-core --all-features`，可运行全部 **117 个测试**，目前**全部通过、零告警**。

### 场景矩阵

| 迁移场景 | MySQL | PostgreSQL | SQLite | 注意事项 |
| --- | --- | --- | --- | --- |
| 创建缺失表 / 已一致 | ✅ 基线 | ✅ 基线 | ✅ 基线 | 先 `create_missing_tables`，一致后计划为空 |
| 新增列 | ✅ `adds_a_column_missing_from_the_table` | ✅ `adds_a_column_missing_from_the_table` | ✅ `adds_a_column_without_rebuilding` | SQLite 直接 ADD，不走重建 |
| 删除列 | ✅ `drops_a_column_…` | ✅ `drops_a_column_…` | ✅ `drops_a_column_without_rebuilding` | 破坏性——不可逆 |
| 加宽类型（`int` → `bigint`） | ✅ `widens_a_column_type` | ✅ `widens_a_column_type` | ✅ `widening_i32_to_i64_is_not_a_change` | SQLite 同亲和族 → 完全不产生语句 |
| 收紧为 `NOT NULL` | ✅ `makes_a_nullable_column_required` | ✅ `makes_a_required_column_nullable` | ✅（重建表） | SQLite 走重建 |
| 放宽为可空 | ◐ 单元 | ✅ `makes_a_required_column_nullable` | ✅（重建表） | MySQL 重复整列定义 |
| 修改默认值 | ✅ `adds_a_default_value` | ✅ `adds_a_default_value` | ✅ `adds_a_default_value_by_rebuilding` / `changes_a_default_value_by_rebuilding` | SQLite 走重建 |
| 新增 `NOT NULL` 列（有默认值） | ✅ `adds_a_not_null_column_with_a_default` | ◐ 单元 | ✅ `adds_a_not_null_column_with_a_default` | — |
| 新增 `NOT NULL` 列（无默认值） | ✅ `adds_a_required_column_without_a_default` | ◐ 单元 | ⚠️ 报错（已记录） | MySQL 严格模式**静默**填 `''`/`0`；SQLite 直接拒绝 |
| 新增唯一索引 / 约束 | ✅ `adds_a_missing_unique_index` | ✅ `adds_a_missing_unique_constraint` | ✅ `adds_a_missing_unique_index_by_rebuilding` | SQLite 走重建 |
| 删除唯一索引 / 约束 | ✅ `drops_an_index_…` | ✅ `drops_a_unique_constraint` | ✅（重建表） | — |
| 新增**普通（非唯一）索引** | ⚠️ 未覆盖 | ⚠️ 未覆盖 | ⚠️ 未覆盖 | `parse_create_table` 忽略表级 `INDEX` 子句，预期 schema 永远装不进它；只有**删除**方向可达 |
| 删除普通索引 | ✅ `drops_a_plain_index` | ✅ `drops_a_plain_index` | ✅ `drops_an_index_by_rebuilding` | PG 索引须用单独的 `CREATE INDEX` 建，不能写在 `CREATE TABLE` 内 |
| 新增 `AUTO_INCREMENT` / identity | ✅ `adds_auto_increment_to_a_primary_key` | ✅ `adds_an_identity_to_a_primary_key` | — | SQLite `INTEGER PRIMARY KEY` 本就自增 |
| 删除默认值 | ◐ 单元 | ✅ `drops_a_default_value` | ✅（重建表） | — |
| 重建表保留已有数据 | — | — | ✅ `rebuilding_keeps_the_rows` | 仅 SQLite 重建路径 |
| 并发锁（只跑一次） | ✅ `a_second_instance_waits…`、`migrate_*` | ✅ `a_second_instance_skips…` | ◐ 单元 | MySQL `GET_LOCK`；PG `skip_if_locked`；SQLite `busy_timeout` |
| 幂等（应用后再计划为空） | ✅ `a_fully_migrated_database_plans_nothing` | ✅ 同上 | ✅ 同上 | 声明式迁移总能收敛 |

图例：✅ 有端到端测试覆盖（通过）· ◐ 仅由单元测试在计划器层覆盖 · ⚠️ 已知缺口或已记录的报错 · — 不适用。

需要特别指出的一处结构性缺口：由于 `parse_create_table` 不读取表级 `INDEX` 子句，经 SeaORM `#[sea_orm(indexed)]` 声明的普通（非唯一）索引对预期 schema 不可见，因此迁移**永远无法"新增"它**——只有当它已存在于表中时才会被识别并**删除**。这是 schema 解析器的限制，而非 DDL 计划器的限制（后者在手动构造 diff 时仍能正确输出 `ADD INDEX`）。

## License

MIT
