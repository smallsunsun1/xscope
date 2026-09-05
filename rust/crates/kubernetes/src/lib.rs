pub mod api;
pub mod autoscaling;
pub mod controller;
pub mod pool;
pub mod resources;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("resource is not owned by this ModelDeployment")]
    OwnershipConflict,
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
