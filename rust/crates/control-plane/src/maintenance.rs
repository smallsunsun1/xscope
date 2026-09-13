//! Local administrative CLI. No network route, no migration, no SQL escape hatch.
use crate::{config::Config, error::ServiceResult, repository::Repository};
use sea_orm::{ConnectOptions, Database};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Read;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    RegisterCluster {
        request: crate::clusters::RegisterCluster,
    },
    PutDesired {
        id: String,
        request: xscope_domain::cluster::DesiredState,
    },
    PutModel {
        id: String,
        request: xscope_domain::catalog::PutModel,
    },
    BindPool {
        request: xscope_domain::traffic::PoolBinding,
    },
    CreateProject {
        project: xscope_domain::Project,
    },
    CreateKey {
        request: xscope_domain::ApiKeyRequest,
    },
    RevokeKey {
        id: String,
    },
    PoolStatus {
        id: String,
    },
    Snapshot,
}
pub async fn run() -> Result<(), ()> {
    if std::env::var("XSCOPE_LOCAL_MAINTENANCE").as_deref() != Ok("enabled") {
        return Err(());
    }
    let mut input = Vec::new();
    std::io::stdin()
        .take(262145)
        .read_to_end(&mut input)
        .map_err(|_| ())?;
    if input.len() > 262144 {
        return Err(());
    }
    let command: Command = serde_json::from_slice(&input).map_err(|_| ())?;
    let config = Config::from_env().map_err(|_| ())?;
    let mut options = ConnectOptions::new(config.database_url);
    options.max_connections(1).sqlx_logging(false);
    let repository = Repository::new(Database::connect(options).await.map_err(|_| ())?);
    let value = execute(&repository, command, &config.route_pools)
        .await
        .map_err(|_| ())?;
    println!("{}", serde_json::to_string(&value).map_err(|_| ())?);
    Ok(())
}
async fn execute(
    repo: &Repository,
    command: Command,
    legacy: &[xscope_domain::RoutePool],
) -> ServiceResult<Value> {
    const ACTOR: &str = "kubernetes-maintenance";
    Ok(match command {
        Command::RegisterCluster { request } => repo.register_cluster(request, ACTOR).await?,
        Command::PutDesired { id, request } => {
            repo.put_cluster_desired(&id, request, ACTOR).await?
        }
        Command::PutModel { id, request } => json!(repo.put_model(&id, request).await?),
        Command::BindPool { request } => repo.bind_pool(request, ACTOR, legacy).await?,
        Command::CreateProject { project } => json!(repo.create_project(project).await?),
        Command::CreateKey { request } => json!(repo.issue_api_key(request).await?),
        Command::RevokeKey { id } => {
            repo.revoke_api_key(&id).await?;
            json!({"revoked":true})
        }
        Command::PoolStatus { id } => repo.managed_pool_status(&id).await?,
        Command::Snapshot => json!(repo.gateway_snapshot().await?),
    })
}
