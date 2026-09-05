pub mod api;
pub mod controller;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("resource is not owned by this ModelDeployment")]
    OwnershipConflict,
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
