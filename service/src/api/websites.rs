//! Website management endpoints.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use crate::api::AppState;
use crate::error::ApiError;
use crate::models::{CreateWebsite, UpdateWebsite, Website};
use crate::storage::WebsitePatch;

/// `GET /api/websites` — list websites.
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<Website>>, ApiError> {
    Ok(Json(state.storage.websites()?))
}

/// `POST /api/websites` — create a website.
pub async fn create(
    State(state): State<AppState>,
    Json(request): Json<CreateWebsite>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("name must not be empty".into()));
    }
    let domain = request
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);

    let website = state.storage.create_website(
        name,
        domain.as_deref(),
        state.config.retention_days,
        state.config.country_header.as_deref(),
    )?;
    // **The switches may be set right away** — a site that is born with them
    // does not have to be born and then changed.
    let website = match request.settings.as_ref() {
        Some(settings) => {
            if let Err(message) = crate::settings::validate(settings) {
                return Err(ApiError::BadRequest(message));
            }
            state.storage.update_website(
                &website.id,
                &WebsitePatch {
                    settings: Some(settings.clone()),
                    ..Default::default()
                },
            )?
        }
        None => website,
    };

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": website.id,
            "websiteId": website.id,
            "name": website.name,
            "domain": website.domain,
            "createdAt": website.created_at,
            "updatedAt": website.updated_at,
            "retentionDays": website.retention_days,
            "settings": website.settings,
        })),
    ))
}

/// `GET /api/websites/{id}` — one website.
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Website>, ApiError> {
    state
        .storage
        .website(&id)?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("website not found: {id}")))
}

/// `PUT /api/websites/{id}` — update a website.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateWebsite>,
) -> Result<Json<Website>, ApiError> {
    // **The switches are checked** — a name that is not ours is a mistake,
    // and it is caught here rather than silently doing nothing.
    if let Some(settings) = request.settings.as_ref() {
        if let Err(message) = crate::settings::validate(settings) {
            return Err(ApiError::BadRequest(message));
        }
    }
    let patch = WebsitePatch {
        name: request.name.clone(),
        domain: request.domain.clone(),
        retention_days: request.retention_days,
        country_header: request.country_header.clone(),
        settings: request.settings.clone(),
    };
    Ok(Json(state.storage.update_website(&id, &patch)?))
}

/// `DELETE /api/websites/{id}` — delete a website and all of its data.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state.storage.delete_website(&id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound(format!("website not found: {id}")))
    }
}
