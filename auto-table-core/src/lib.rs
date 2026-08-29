//! 自动建表核心库
//!
//! 提供 `#[auto_table]` 宏所需的基础类型和收集机制。
//!
//! 错误分层：本库作为可复用库，通过 `thiserror` 暴露精确的
//! [`TableError`]，供上层（应用/宏生成代码）用 `anyhow` 或自定义
//! 错误类型吸收传播。

use sea_orm::sea_query::TableCreateStatement;
use sea_orm::{DatabaseConnection, DbBackend};

/// 自动建表相关错误（库层精确错误类型）
///
/// 库的公共 API 返回本类型，调用方可以 `match` 区分失败原因；
/// 上游应用层可经 `?` 自动装箱为 `anyhow::Error` 传播。
#[derive(Debug, thiserror::Error)]
pub enum TableError {
    /// 查询已存在表列表失败（底层数据库错误）
    #[error("查询已存在的表失败: {0}")]
    QueryExistingFailed(#[from] sea_orm::DbErr),
    /// 创建某张表失败（携带表名便于定位）
    #[error("创建表 `{table}` 失败: {source}")]
    CreateFailed {
        /// 失败的表名
        table: String,
        /// 底层数据库错误
        #[source]
        source: sea_orm::DbErr,
    },
    /// 不支持的数据库后端
    #[error("不支持的数据库后端: {0:?}")]
    UnsupportedBackend(DbBackend),
}

/// 用于 inventory 收集的包装结构
///
/// 每个使用 `#[auto_table]` 标记的 Entity 都会生成一个此类型的实例，
/// 通过 `inventory` 在编译时自动收集。
pub struct TableRegistration {
    pub create_fn: fn(DbBackend) -> TableCreateStatement,
}

// 使用 inventory 收集所有注册的表
inventory::collect!(TableRegistration);

/// 获取所有注册的表创建语句
pub fn get_all_table_statements(backend: DbBackend) -> Vec<TableCreateStatement> {
    inventory::iter::<TableRegistration>
        .into_iter()
        .map(|reg| (reg.create_fn)(backend))
        .collect()
}

/// 从 TableCreateStatement 中提取表名
pub fn get_table_name(stmt: &TableCreateStatement) -> Option<String> {
    stmt.get_table_name()
        .map(|table_ref| table_ref.sea_orm_table().to_string())
}

/// 查询数据库中已存在的表名列表
///
/// 遇到不支持的数据库后端时返回 [`TableError::UnsupportedBackend`]。
pub async fn get_existing_tables(
    db: &DatabaseConnection,
    backend: DbBackend,
) -> Result<Vec<String>, TableError> {
    use sea_orm::ConnectionTrait;

    let sql = match backend {
        DbBackend::MySql => {
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = DATABASE()"
        }
        DbBackend::Postgres => {
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public'"
        }
        DbBackend::Sqlite => {
            "SELECT name FROM sqlite_master WHERE type='table'"
        }
        other => return Err(TableError::UnsupportedBackend(other)),
    };

    let result: Vec<sea_orm::QueryResult> = db
        .query_all_raw(sea_orm::Statement::from_string(backend, sql.to_string()))
        .await?;

    let tables = result
        .iter()
        .filter_map(|row| row.try_get_by_index::<String>(0).ok())
        .collect();

    Ok(tables)
}

// 重新导出过程宏，使用户只需依赖 `auto_table_core` 一个 crate 即可使用
// `#[auto_table]` 与 `#[auto_create]`（无需单独引入 `auto_table_derive`）。
pub use auto_table_derive::{auto_create, auto_table};
