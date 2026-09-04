//! Prints the CREATE TABLE statements SeaORM generates for representative entities
//!
//! This exists to inspect the exact output that the schema parser has to
//! handle, across the backends we care about:
//!
//! ```sh
//! cargo run --example dump_create_table -p auto-table-core
//! ```

use sea_orm::{DbBackend, Schema};

/// Single-column primary key, covering the common column types
mod users {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sample_users")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i32,
        #[sea_orm(unique)]
        pub email: String,
        pub nickname: Option<String>,
        pub age: i32,
        pub score: i64,
        pub balance: Decimal,
        pub active: bool,
        pub created_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Composite primary key, plus a column with an explicit default value
mod memberships {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "sample_memberships")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: i32,
        #[sea_orm(primary_key, auto_increment = false)]
        pub group_id: i32,
        #[sea_orm(default_value = "member")]
        pub role: String,
        pub note: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

fn main() {
    for backend in [DbBackend::MySql, DbBackend::Postgres, DbBackend::Sqlite] {
        println!("##################### {backend:?} #####################");
        for sql in [
            dump(backend, users::Entity),
            dump(backend, memberships::Entity),
        ] {
            println!("{sql}");
            println!();
        }
    }
}

fn dump<E: sea_orm::EntityTrait>(backend: DbBackend, entity: E) -> String {
    let stmt = Schema::new(backend).create_table_from_entity(entity);
    backend.build(&stmt).sql
}
