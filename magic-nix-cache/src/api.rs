//! Action API.
//!
//! This API is intended to be used by nix-installer-action.

use attic::nix_store::StorePath;
use axum::{extract::Extension, routing::post, Json, Router};
use axum_macros::debug_handler;
use serde::{Deserialize, Serialize};

use super::State;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize)]
struct WorkflowStartResponse {
    num_original_paths: usize,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowFinishResponse {
    num_original_paths: usize,
    num_final_paths: usize,
    num_new_paths: usize,
}

pub fn get_router() -> Router {
    Router::new()
        .route("/api/workflow-start", post(workflow_start))
        .route("/api/workflow-finish", post(workflow_finish))
        .route("/api/enqueue-paths", post(post_enqueue_paths))
}

/// Record existing paths.
#[debug_handler]
async fn workflow_start(Extension(state): Extension<State>) -> Result<Json<WorkflowStartResponse>> {
    tracing::info!("Workflow started");
    let mut original_paths = state.original_paths.lock().await;
    *original_paths = crate::util::get_store_paths(&state.store).await?;

    let reply = WorkflowStartResponse {
        num_original_paths: original_paths.len(),
    };

    state
        .metrics
        .num_original_paths
        .set(reply.num_original_paths);

    Ok(Json(reply))
}

/// Push new paths and shut down.
async fn workflow_finish(
    Extension(state): Extension<State>,
) -> Result<Json<WorkflowFinishResponse>> {
    tracing::info!("Workflow finished");

    let original_paths = state.original_paths.lock().await;
    let final_paths = crate::util::get_store_paths(&state.store).await?;
    let new_paths = final_paths
        .difference(&original_paths)
        .cloned()
        .map(|path| state.store.follow_store_path(path).map_err(Error::Attic))
        .collect::<Result<Vec<_>>>()?;

    let num_original_paths = original_paths.len();
    let num_final_paths = final_paths.len();
    let num_new_paths = new_paths.len();

    // NOTE(cole-h): If we're substituting from an upstream cache, those paths won't have the
    // post-build-hook run on it, so we diff the store to ensure we cache everything we can.
    tracing::info!("Diffing the store and uploading any new paths before we shut down");
    enqueue_paths(&state, new_paths).await?;

    if let Some(gha_cache) = &state.gha_cache {
        tracing::info!("Waiting for GitHub action cache uploads to finish");
        gha_cache.shutdown().await?;
    }

    if let Some(sender) = state.shutdown_sender.lock().await.take() {
        sender
            .send(())
            .map_err(|_| Error::Internal("Sending shutdown server message".to_owned()))?;
    }

    if let Some(attic_state) = state.flakehub_state.write().await.take() {
        tracing::info!("Waiting for FlakeHub cache uploads to finish");
        let paths = attic_state.push_session.wait().await?;
        tracing::warn!(?paths, "pushed these paths");
    }

    // NOTE(cole-h): see `init_logging`
    let logfile = std::env::temp_dir().join("magic-nix-cache-tracing.log");
    let logfile_contents = std::fs::read_to_string(logfile)?;
    println!("Every log line throughout the lifetime of the program:");
    println!("\n{logfile_contents}\n");

    let reply = WorkflowFinishResponse {
        num_original_paths,
        num_final_paths,
        num_new_paths,
    };

    state
        .metrics
        .num_original_paths
        .set(reply.num_original_paths);
    state.metrics.num_final_paths.set(reply.num_final_paths);
    state.metrics.num_new_paths.set(reply.num_new_paths);

    Ok(Json(reply))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueuePathsRequest {
    pub store_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueuePathsResponse {}

/// Schedule paths in the local Nix store for uploading.
#[tracing::instrument(skip_all)]
async fn post_enqueue_paths(
    Extension(state): Extension<State>,
    Json(req): Json<EnqueuePathsRequest>,
) -> Result<Json<EnqueuePathsResponse>> {
    tracing::info!("Enqueueing {:?}", req.store_paths);

    let store_paths = req
        .store_paths
        .iter()
        .map(|path| state.store.follow_store_path(path).map_err(Error::Attic))
        .collect::<Result<Vec<_>>>()?;

    enqueue_paths(&state, store_paths).await?;

    Ok(Json(EnqueuePathsResponse {}))
}

async fn enqueue_paths(state: &State, store_paths: Vec<StorePath>) -> Result<()> {
    if let Some(gha_cache) = &state.gha_cache {
        gha_cache
            .enqueue_paths(state.store.clone(), store_paths.clone())
            .await?;
    }

    if let Some(flakehub_state) = &*state.flakehub_state.read().await {
        tracing::warn!("enqueuing {:?} for flakehub", store_paths);
        crate::flakehub::enqueue_paths(flakehub_state, store_paths).await?;
    }

    Ok(())
}
