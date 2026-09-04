# auto-table

[English](README.md) | [中文文档](README.zh-CN.md)

基于 [SeaORM](https://crates.io/crates/sea-orm) 的自动建表工具库。通过属性宏在编译期收集所有实体，并在应用启动时自动创建数据库中缺失的表。

## 组成

本 workspace 包含两个 crate：

| Crate | 说明 |
| --- | --- |
| [`auto-table-core`](auto-table-core) | 核心库，提供 `#[auto_table]` / `#[auto_create]` 宏的运行时支持与错误类型 |
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

> 特性只决定编译进哪些**数据库驱动**。后端在运行时由 `DbBackend` 判别，因此查询已有表等逻辑对三个后端始终可用。

## 支持的后端

- MySQL
- PostgreSQL
- SQLite

## 错误处理

核心库通过 `thiserror` 暴露精确的 [`auto_table_core::TableError`](auto-table-core/src/lib.rs)，上层应用可经 `?` 自动装箱为 `anyhow::Error` 传播。

## 路线图

- [ ] **数据库迁移（migration）** — 目前只会创建缺失的表，不会对已存在的表做结构变更。计划支持：
  - 对比实体定义与线上表结构，自动生成 `ALTER TABLE` 语句
  - 新增/删除列，修改列类型与约束，索引与唯一约束变更
  - dry-run 预览迁移 SQL，确认后再执行
  - 危险操作分级：删列、类型收紧等可能丢数据的操作需显式确认，默认不自动执行
  - 并发安全：多实例同时启动时通过数据库锁（`GET_LOCK` / `pg_advisory_lock` / SQLite 独占事务）保证只有一个实例执行迁移
- [ ] **SQLite 重建表** — SQLite 不支持 `MODIFY COLUMN`，删除列需 3.35+，其结构变更必须走「建新表 → 拷贝数据 → 删旧表 → 重命名」流程，需单独实现
- [ ] 迁移回滚 — 本库迁移是声明式的（每次对比当前状态与目标状态），难以自动生成 down：`DROP COLUMN` 后数据已丢失，且 MySQL / SQLite 的 DDL 不支持在事务中回滚。计划先落地「失败时按逆操作尽力回滚」，有损步骤明确报错而非静默继续
- [x] 更细粒度的后端特性开关（按需启用 MySQL / PostgreSQL / SQLite）

## License

MIT
