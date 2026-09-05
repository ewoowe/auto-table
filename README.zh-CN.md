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
- **SQLite** 没有命名锁，依赖其自身的写锁；加锁只是额外设置一个 `busy_timeout`，让并发实例排队而不是立刻收到 `SQLITE_BUSY`。

无论哪种后端，**拿到锁之后都会重新生成一次计划**：等待锁的这段时间里另一个实例可能已经迁移完毕，重放过期计划只会失败。

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

#### 危险操作分级（设计中，尚未实现）

两种"危险"性质不同，防护方式也不该相同：

- **Caution（可能失败）**：迁移被拒绝——MySQL 在严格模式下整条语句失败，SQLite 则是重建失败并整体回滚，**两者的数据都原封不动**。它们不会毁数据，只会让迁移中断。
- **Destructive（可能毁数据）**：无条件成功、**数据永久丢失且无法撤销**，两个后端皆是如此。

只有后者需要人工授权，前者只需要提示。拟定分级如下，MySQL 与 SQLite 两列均为实测结果：

| 变更 | 级别 | MySQL | SQLite | PostgreSQL |
| --- | --- | --- | --- | --- |
| 新增可空列 | Safe | 填 `NULL` | 填 `NULL` | 待测 |
| 新增 `NOT NULL` 列（有默认值） | Safe | 填默认值 | 填默认值 | 待测 |
| 加宽类型（如 `int` → `bigint`） | Safe | 无损 | 不产生语句，亲和性相同 | 待测 |
| 修改默认值 | Safe | 不影响已有行 | 重建表，已有行保持原值 | 待测 |
| 删除索引 | Safe | 不影响数据 | 重建表，数据不受影响 | 待测 |
| 新增 `NOT NULL` 列（无默认值） | Caution | **静默填 `''` 或 `0`** | 直接报错 | 待测 |
| 收窄类型 | Caution | 严格模式下报错 | 按亲和性转换，无法转换的值原样保留 | 待测 |
| 收紧为 `NOT NULL` | Caution | 存在 `NULL` 时报错 | 重建失败并回滚，原表完好 | 待测 |
| 新增唯一索引/约束 | Caution | 存在重复值时报错 | 重建失败并回滚，原表完好 | 待测 |
| **删除列** | **Destructive** | **数据永久丢失，不可回滚** | **数据永久丢失，不可回滚** | 待测 |

拟定的 API（尚未实现，仍可能调整）：

```rust
// 每个变更可自报风险
pub enum Risk { Safe, Caution, Destructive }

// 默认：发现破坏性变更就拒绝执行任何语句
apply_migrations(&db, &plan).await?;

// 显式授权后才执行
apply_migrations(&db, &plan).allow_destructive().await?;
```

两处设计取舍：

- **先检查、后执行**：在跑任何语句之前先扫描整个计划，发现未授权的破坏性变更就立即报错、一条都不执行。MySQL 的 DDL 不能回滚，"执行到一半才发现危险"会把数据库留在半成品状态；对 SQLite 而言也能省下一次白白的大表重建。
- **破坏性只判删除列**：宁可标准单一也要可信。若把类型收窄也算破坏性，几乎每次改字段都要授权，用户很快养成习惯性确认，防护便形同虚设。

> PostgreSQL 一列为预留：该后端尚未支持迁移，其 ALTER 语法与另两者差异较大，行为待实测。同一变更在两个后端上的危险程度可能不同，最典型的是「新增 `NOT NULL` 列但无默认值」——MySQL 静默填充，SQLite 直接报错。

> 本节为设计草案，尚未实现。

## 支持的后端

自动建表与迁移均支持以下三者。

- MySQL
- PostgreSQL
- SQLite

## 错误处理

核心库通过 `thiserror` 暴露精确的 [`auto_table_core::TableError`](auto-table-core/src/lib.rs)，上层应用可经 `?` 自动装箱为 `anyhow::Error` 传播。

## 路线图

- [ ] **数据库迁移（migration）** — 三个后端均可用（见「4. 迁移已存在的表」），仍待完善：
  - 危险操作分级：删除列等不可逆操作需显式授权，未授权则拒绝执行整个计划（方案见上文「危险操作分级（设计中）」）
  - 并发安全：多实例同时启动时通过数据库锁保证只有一个实例执行迁移（已完成：MySQL 用 `GET_LOCK`，SQLite 依赖写锁 + `busy_timeout`，PostgreSQL 用 `pg_advisory_lock`；见「5. 并发安全」）
- [x] **SQLite 迁移（含重建表）** — SQLite 不支持 `MODIFY COLUMN`，故列定义变更与索引/约束变更都走「建新表 → 拷贝数据 → 删旧表 → 重命名」流程，在单个事务内完成、失败自动回滚（详见「SQLite 的迁移场景」）
- [ ] 迁移回滚 — 本库迁移是声明式的（每次对比当前状态与目标状态），难以自动生成 down：`DROP COLUMN` 后数据已丢失，且 MySQL 的 DDL 不支持在事务中回滚（SQLite 则可以，已实测）。计划先落地「失败时按逆操作尽力回滚」，有损步骤明确报错而非静默继续
- [x] 更细粒度的后端特性开关（按需启用 MySQL / PostgreSQL / SQLite）

## License

MIT
