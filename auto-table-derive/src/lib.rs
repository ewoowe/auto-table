use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, ItemFn, ItemStruct};

// Note: a proc-macro crate cannot directly re-export ordinary library items,
// so the procedural macros are re-exported through `auto_table_core`
// (`pub use auto_table_derive::{auto_create, auto_table};`).
// Users only need to depend on `auto_table_core`.

/// Attribute macro for automatic table creation
///
/// Apply it to the `Model` struct of a SeaORM Entity; it generates the
/// inventory registration code automatically.
///
/// # Example
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

    // Keep the original struct definition
    let original = &input;

    // Generate the inventory registration code
    let expanded = quote! {
        #original

        // Auto-generated inventory registration
        // Uses the TableRegistration type defined in auto_table_core
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

/// Attribute macro that injects automatic table creation
///
/// Apply it to a function that initializes the database. The argument is the
/// name of the `DatabaseConnection` variable in the function body. The macro
/// locates the `let <db> = ...` binding and injects the table-creation call
/// immediately after it, so the call runs right after the connection is
/// established and before `db` is moved. This does not depend on how many
/// trailing statements the function body has.
///
/// # Example
/// ```ignore
/// #[auto_create(db)]
/// pub async fn init_pool(database_url: &str) -> anyhow::Result<()> {
///     let db = Database::connect(database_url).await?;
///     db.ping().await?;
///     DB.set(db).expect("...");
///     Ok(())
/// }
/// ```
///
/// Equivalent to writing manually:
/// ```ignore
/// pub async fn init_pool(database_url: &str) -> anyhow::Result<()> {
///     let db = Database::connect(database_url).await?;
///     let __auto_table_report = create_tables(&db).await?;  // injection point
///     db.ping().await?;
///     DB.set(db).expect(...);     // db is only moved here
///     Ok(())
/// }
///
/// async fn create_tables(
///     db: &sea_orm::DatabaseConnection,
/// ) -> Result<auto_table_core::TableCreationReport, auto_table_core::TableError> {
///     auto_table_core::create_missing_tables(db).await
/// }
/// ```
///
/// The table-creation logic is implemented in
/// [`auto_table_core::create_missing_tables`], which performs no logging and
/// instead returns a [`auto_table_core::TableCreationReport`] describing which
/// tables already existed and which were created.
///
/// The injected statement binds that report to a fixed local variable named
/// `__auto_table_report` (of type `auto_table_core::TableCreationReport`),
/// which is in scope for the statements that follow the injection point (i.e.
/// the rest of the function body). Its name starts with an underscore, so it
/// does not trigger an unused-variable warning if you do not consume it.
///
/// If no `let <db> = ...` binding is found (e.g. `db` is a function
/// parameter), the call is injected at the very beginning of the function body.
#[proc_macro_attribute]
pub fn auto_create(attr: TokenStream, item: TokenStream) -> TokenStream {
    let db_var = parse_macro_input!(attr as syn::Ident);
    let mut input = parse_macro_input!(item as ItemFn);

    let fn_name = &input.sig.ident;
    // Prefix the generated helper function name to avoid collisions
    let create_tables_fn = format_ident!("__auto_create_tables_{}", fn_name);

    let call_stmt: syn::Stmt = syn::parse_quote! {
        let __auto_table_report = #create_tables_fn(&#db_var).await?;
    };

    // Inject the call right after the `let <db_var> = ...` binding, instead of
    // guessing a fixed number of trailing statements. The report is then in
    // scope for the rest of the function body.
    let stmts = &input.block.stmts;
    let mut new_stmts: Vec<syn::Stmt> = Vec::with_capacity(stmts.len() + 1);

    let mut inserted = false;
    for stmt in stmts.iter() {
        new_stmts.push(stmt.clone());
        if !inserted && is_db_binding(stmt, &db_var) {
            new_stmts.push(call_stmt.clone());
            inserted = true;
        }
    }

    // Fallback: no `let <db_var> = ...` binding found (e.g. `db` is a function
    // parameter). Inject at the very beginning of the body.
    if !inserted {
        new_stmts.insert(0, call_stmt);
    }

    input.block.stmts = new_stmts;

    let expanded = quote! {
        #input

        /// Auto-generated table-creation function
        ///
        /// Delegates to `auto_table_core::create_missing_tables`, returning a
        /// `auto_table_core::TableCreationReport` (the tables that already
        /// existed and the tables created in this run). Failures are reported
        /// as the precise library-level error `auto_table_core::TableError`,
        /// which the caller (application layer) can box into an anyhow error via `?`.
        async fn #create_tables_fn(db: &sea_orm::DatabaseConnection) -> Result<auto_table_core::TableCreationReport, auto_table_core::TableError> {
            auto_table_core::create_missing_tables(db).await
        }
    };

    expanded.into()
}

/// Attribute macro that injects automatic schema migration.
///
/// Apply it to a function that initializes the database. The argument is the
/// name of the `DatabaseConnection` variable in the function body. The macro
/// locates the `let <db> = ...` binding and injects the migration call
/// immediately after it, so the migration runs right after the connection is
/// established and before `db` is moved. This does not depend on how many
/// trailing statements the function body has.
///
/// # Example
/// ```ignore
/// #[auto_migrate(db)]
/// pub async fn init_pool(database_url: &str) -> anyhow::Result<()> {
///     let db = Database::connect(database_url).await?;
///     db.ping().await?;
///     DB.set(db).expect("...");
///     Ok(())
/// }
/// ```
///
/// Equivalent to writing manually:
/// ```ignore
/// pub async fn init_pool(database_url: &str) -> anyhow::Result<()> {
///     let db = Database::connect(database_url).await?;
///     let __auto_table_migration = migrate_tables(&db).await?;  // injection point
///     db.ping().await?;
///     DB.set(db).expect("...");
///     Ok(())
/// }
///
/// async fn migrate_tables(
///     db: &sea_orm::DatabaseConnection,
///     options: auto_table_core::MigrateOptions,
/// ) -> Result<auto_table_core::MigrationOutcome, auto_table_core::TableError> {
///     auto_table_core::migrate(db, options).await
/// }
/// ```
///
/// The migration logic is implemented in [`auto_table_core::migrate`], which
/// performs no logging and instead returns the [`auto_table_core::MigrationOutcome`]
/// (whether the plan was applied or skipped). The injected statement binds that
/// outcome to a fixed local variable named `__auto_table_migration`; its name
/// starts with an underscore, so it does not trigger an unused-variable warning
/// if you do not consume it.
///
/// By default (no second argument) the migration refuses any destructive change
/// (e.g. dropping a column) and returns
/// `auto_table_core::TableError::DestructiveChangesBlocked`. To allow it, pass
/// options as the second macro argument, e.g.
/// `#[auto_migrate(db, MigrateOptions::default().allow_destructive())]`, or call
/// [`auto_table_core::migrate`] / [`auto_table_core::ensure_schema`] directly.
/// Arguments for [`auto_migrate`]: `<db_var>` or `<db_var>, <options_expr>`.
struct AutoMigrateArgs {
    db: syn::Ident,
    options: Option<syn::Expr>,
}

impl syn::parse::Parse for AutoMigrateArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let db: syn::Ident = input.parse()?;
        let options = if input.peek(syn::Token![,]) {
            input.parse::<syn::Token![,]>()?;
            Some(input.parse::<syn::Expr>()?)
        } else {
            None
        };
        Ok(AutoMigrateArgs { db, options })
    }
}

#[proc_macro_attribute]
pub fn auto_migrate(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as AutoMigrateArgs);
    let db_var = args.db;
    let options = match args.options {
        Some(expr) => quote!(#expr),
        None => quote!(auto_table_core::MigrateOptions::default()),
    };
    let mut input = parse_macro_input!(item as ItemFn);

    let fn_name = &input.sig.ident;
    // Prefix the generated helper function name to avoid collisions
    let migrate_fn = format_ident!("__auto_migrate_tables_{}", fn_name);

    let call_stmt: syn::Stmt = syn::parse_quote! {
        let __auto_table_migration = #migrate_fn(&#db_var, #options).await?;
    };

    // Inject the call right after the `let <db_var> = ...` binding, instead of
    // guessing a fixed number of trailing statements. The outcome is then in
    // scope for the rest of the function body.
    let stmts = &input.block.stmts;
    let mut new_stmts: Vec<syn::Stmt> = Vec::with_capacity(stmts.len() + 1);

    let mut inserted = false;
    for stmt in stmts.iter() {
        new_stmts.push(stmt.clone());
        if !inserted && is_db_binding(stmt, &db_var) {
            new_stmts.push(call_stmt.clone());
            inserted = true;
        }
    }

    // Fallback: no `let <db_var> = ...` binding found (e.g. `db` is a function
    // parameter). Inject at the very beginning of the body.
    if !inserted {
        new_stmts.insert(0, call_stmt);
    }

    input.block.stmts = new_stmts;

    let expanded = quote! {
        #input

        /// Auto-generated schema-migration function
        ///
        /// Delegates to `auto_table_core::migrate` with the caller-supplied
        /// options (or `MigrateOptions::default()`), returning the
        /// `auto_table_core::MigrationOutcome` (Applied or Skipped). Failures
        /// are reported as the precise library-level error
        /// `auto_table_core::TableError`, which the caller (application layer) can
        /// box into an anyhow error via `?`.
        async fn #migrate_fn(
            db: &sea_orm::DatabaseConnection,
            options: auto_table_core::MigrateOptions,
        ) -> Result<auto_table_core::MigrationOutcome, auto_table_core::TableError> {
            auto_table_core::migrate(db, options).await
        }
    };

    expanded.into()
}

/// Returns `true` if `stmt` is a `let` binding whose pattern binds exactly the
/// identifier `var` (either directly as `let var = ...` or with a type
/// annotation as `let var: Type = ...`).
fn is_db_binding(stmt: &syn::Stmt, var: &syn::Ident) -> bool {
    let syn::Stmt::Local(local) = stmt else {
        return false;
    };
    match &local.pat {
        syn::Pat::Ident(pat) => pat.ident == *var,
        syn::Pat::Type(pat) => matches!(&*pat.pat, syn::Pat::Ident(inner) if inner.ident == *var),
        _ => false,
    }
}
