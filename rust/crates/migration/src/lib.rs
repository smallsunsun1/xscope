pub use sea_orm_migration::prelude::*;

mod m20260905_000001_platform;
mod m20260905_000002_billing;
mod m20260905_000003_identity;
mod m20260905_000004_routing;
mod m20260905_000005_reservations;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260905_000001_platform::Migration),
            Box::new(m20260905_000002_billing::Migration),
            Box::new(m20260905_000003_identity::Migration),
            Box::new(m20260905_000004_routing::Migration),
            Box::new(m20260905_000005_reservations::Migration),
        ]
    }
}
