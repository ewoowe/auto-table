use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, ItemFn, ItemStruct};

// 注意：proc-macro crate 不能直接 re-export 普通库
// 用户需要同时依赖 auto_table_core 和 auto_table_derive

/// 自动建表属性宏
///
/// 用在 SeaORM Entity 的 Model 结构体上，自动生成 inventory 注册代码
///
/// # 示例
/// ```ignore
/// #[auto_table]
/// #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
/// #[sea_orm(table_name = "users")]
/// pub struct Model {
///     // ...
/// }
/// ```
#[proc_macro_attribute]
pub fn auto_table(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemStruct);
    
    // 保留原始结构体定义
    let original = &input;
    
    // 生成 inventory 注册代码
    let expanded = quote! {
        #original
        
        // 自动生成 inventory 注册
        // 使用 auto_table_core 中定义的 TableRegistration
        inventory::submit! {
            auto_table_core::TableRegistration {
                create_fn: |backend| {
                    sea_orm::Schema::new(backend)
                        .create_table_from_entity(Entity)
                },
            }
        }
    };
    
    expanded.into()
}

/// 自动建表注入属性宏
///
/// 用在包含数据库初始化的函数上，自动在函数体倒数第二条语句前注入建表逻辑。
/// 参数为函数体中 `DatabaseConnection` 变量的名称。
/// 这样最后两条语句（如 `DB.set(db)` 和 `Ok(())`）保持在注入点之后，
/// 避免了 move 和 borrow 的冲突。
///
/// # 示例
/// ```ignore
/// #[auto_create(db)]
/// pub async fn init_pool(database_url: &str) -> anyhow::Result<()> {
///     let db = Database::connect(database_url).await?;
///     db.ping().await?;
///     DB.set(db).expect("...");  // 最后两条语句之一，在注入点之后
///     Ok(())
/// }
/// ```
///
/// 等价于手动编写：
/// ```ignore
/// pub async fn init_pool(database_url: &str) -> anyhow::Result<()> {
///     let db = Database::connect(database_url).await?;
///     db.ping().await?;
///     create_tables(&db).await?;  // 注入点：倒数第二条语句前
///     DB.set(db).expect(...);     // db 在这里才被 move
///     Ok(())
/// }
///
/// async fn create_tables(db: &sea_orm::DatabaseConnection) -> Result<(), sea_orm::DbErr> {
///     let backend = db.get_database_backend();
///     let statements = auto_table_core::get_all_table_statements(backend);
///     let existing_tables = auto_table_core::get_existing_tables(db, backend).await?;
///
///     for mut stmt in statements {
///         let table_name = auto_table_core::get_table_name(&stmt);
///         if let Some(ref name) = table_name {
///             if existing_tables.contains(name) {
///                 rolling_logger::info!("表 `{}` 已存在，跳过建表", name);
///                 continue;
///             }
///         }
///         stmt.if_not_exists();
///         db.execute(&stmt).await?;
///     }
///     Ok(())
/// }
/// ```
#[proc_macro_attribute]
pub fn auto_create(attr: TokenStream, item: TokenStream) -> TokenStream {
    let db_var = parse_macro_input!(attr as syn::Ident);
    let mut input = parse_macro_input!(item as ItemFn);

    let fn_name = &input.sig.ident;
    let create_tables_fn = format_ident!("__auto_create_tables_{}", fn_name);

    // 在函数体倒数数第二条表达式前注入 create_tables 调用
    // 这样最后两条语句（如 DB.set(db) 和 Ok(())）保持在注入点之后
    let stmts = &input.block.stmts;
    let len = stmts.len();

    let call_stmt: syn::Stmt = syn::parse_quote! {
        #create_tables_fn(&#db_var).await?;
    };

    // 取除最后两条之外的所有语句，给某些特殊操作留位置
    let mut new_stmts: Vec<syn::Stmt> = stmts.iter().take(len.saturating_sub(2)).cloned().collect();
    // 注入建表调用
    new_stmts.push(call_stmt);
    // 追加最后两条语句（保持原有顺序）
    if len >= 2 {
        new_stmts.push(stmts[len - 2].clone());
    }
    if let Some(last) = stmts.last() {
        new_stmts.push(last.clone());
    }

    input.block.stmts = new_stmts;

    let expanded = quote! {
        #input

        /// 自动生成的建表函数
        ///
        /// 返回库层精确错误 `auto_table_core::TableError`，
        /// 由调用方（应用层）经 `?` 装箱为 anyhow 错误传播。
        async fn #create_tables_fn(db: &sea_orm::DatabaseConnection) -> Result<(), auto_table_core::TableError> {
            use sea_orm::ConnectionTrait;

            let backend = db.get_database_backend();
            let statements = auto_table_core::get_all_table_statements(backend);

            // 查询数据库中已存在的表
            let existing_tables = auto_table_core::get_existing_tables(db, backend).await?;
            if !existing_tables.is_empty() {
                rolling_logger::info!("数据库中已存在的表: {:?}", existing_tables);
            }

            let mut created_count = 0;
            let mut skipped_count = 0;

            for mut stmt in statements {
                let table_name = auto_table_core::get_table_name(&stmt);
                if let Some(ref name) = table_name {
                    if existing_tables.contains(name) {
                        rolling_logger::info!("表 `{}` 已存在，跳过建表", name);
                        skipped_count += 1;
                        continue;
                    }
                }

                stmt.if_not_exists();
                db.execute(&stmt)
                    .await
                    .map_err(|source| auto_table_core::TableError::CreateFailed {
                        table: auto_table_core::get_table_name(&stmt)
                            .unwrap_or_else(|| "unknown".to_string()),
                        source,
                    })?;
                created_count += 1;
            }

            rolling_logger::info!("数据库表自动创建完成，新建 {} 张，跳过 {} 张", created_count, skipped_count);
            Ok(())
        }
    };

    expanded.into()
}
