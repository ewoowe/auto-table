//! Prints the CREATE TABLE statements SeaORM generates for a representative entity
//!
//! This exists to inspect the exact output that a future schema parser has to
//! handle, across the backends we care about:
//!
//! ```sh
//! cargo run --example dump_create_table -p auto-table-core
//! ```

use sea_orm::entity::prelude::*;
use sea_orm::{DbBackend, Schema};

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

fn main() {
    for backend in [DbBackend::MySql, DbBackend::Postgres, DbBackend::Sqlite] {
        let stmt = Schema::new(backend).create_table_from_entity(Entity);
        println!("===== {backend:?} =====");
        println!("{}", backend.build(&stmt).sql);
        println!();
    }
}
