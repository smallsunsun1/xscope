use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "ledger_entries", schema_name = "xscope")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub transaction_id: String,
    pub billing_account_id: String,
    pub ledger_account: String,
    pub amount_microunits: i64,
    pub currency: String,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::ledger_transaction::Entity",
        from = "Column::TransactionId",
        to = "super::ledger_transaction::Column::Id",
        on_update = "Cascade",
        on_delete = "Restrict"
    )]
    Transaction,
    #[sea_orm(
        belongs_to = "super::billing_account::Entity",
        from = "Column::BillingAccountId",
        to = "super::billing_account::Column::Id",
        on_update = "Cascade",
        on_delete = "Restrict"
    )]
    BillingAccount,
}

impl Related<super::ledger_transaction::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Transaction.def()
    }
}

impl Related<super::billing_account::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::BillingAccount.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
