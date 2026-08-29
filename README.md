# auto-table

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
auto-table-core = "0.1.0"
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

宏会在函数体倒数第二条语句前注入建表逻辑，保证最后两条语句（如 `DB.set(db)` 与 `Ok(())`）在 `db` 被 move 之后仍然顺序正确。

## 支持的后端

- MySQL
- PostgreSQL
- SQLite

## 错误处理

核心库通过 `thiserror` 暴露精确的 [`auto_table_core::TableError`](auto-table-core/src/lib.rs)，上层应用可经 `?` 自动装箱为 `anyhow::Error` 传播。

## License

MIT
