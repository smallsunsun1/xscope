pub use sea_orm_migration::prelude::*;

mod m20260905_000001_platform;
mod m20260905_000002_billing;
mod m20260905_000003_identity;
mod m20260905_000004_routing;
mod m20260905_000005_reservations;
mod m20260905_000006_pending_reservations;
mod m20260905_000007_billing_projections;
mod m20260906_000008_billing_reviews;
mod m20260906_000009_event_workers;
mod m20260906_000010_clusters;
mod m20260906_000011_audit_guards;
mod m20260906_000012_payments;
mod m20260906_000014_loss_waivers;
mod m20260911_000015_model_catalog;
mod m20260911_000016_route_history;
mod m20260912_000017_managed_traffic;
mod m20260913_000018_safe_scaling;

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
            Box::new(m20260905_000006_pending_reservations::Migration),
            Box::new(m20260905_000007_billing_projections::Migration),
            Box::new(m20260906_000008_billing_reviews::Migration),
            Box::new(m20260906_000009_event_workers::Migration),
            Box::new(m20260906_000010_clusters::Migration),
            Box::new(m20260906_000011_audit_guards::Migration),
            Box::new(m20260906_000012_payments::Migration),
            Box::new(m20260906_000013_tax_requests::Migration),
            Box::new(m20260906_000014_loss_waivers::Migration),
            Box::new(m20260911_000015_model_catalog::Migration),
            Box::new(m20260911_000016_route_history::Migration),
            Box::new(m20260912_000017_managed_traffic::Migration),
            Box::new(m20260913_000018_safe_scaling::Migration),
        ]
    }
}
mod m20260906_000013_tax_requests;
