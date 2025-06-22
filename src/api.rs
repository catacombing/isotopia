//! REST API endpoints.

use std::mem;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State as AxumState};
use axum::http::header::HeaderMap;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};
use tracing::error;

use crate::db::{Device, Status};
use crate::{Error, State};

/// Default image output directory.
pub const IMAGE_DIRECTORY: &str = "./images";

/// Get the API's request router.
pub fn router() -> Router<Arc<State>> {
    // Use allow-all cors policy.
    let cors = CorsLayer::new().allow_methods(Any).allow_headers(Any).allow_origin(Any);

    Router::new()
        .route("/requests/pending", get(get_pending))
        .route("/requests/{device}/{md5sum}/status", put(put_status))
        .route("/requests/{device}/{md5sum}/status", get(get_status))
        .route("/requests", post(post_request))
        .route(
            "/requests/{device}/{md5sum}/{alarm_md5sum}/image",
            post(post_image).layer(DefaultBodyLimit::disable()),
        )
        .route("/requests/{device}/{md5sum}/image", get(get_image))
        .layer(cors)
}

/// Get pending build requests.
async fn get_pending(AxumState(state): AxumState<Arc<State>>) -> Response {
    let pending = match state.db.pending().await {
        Ok(pending) => pending,
        Err(err) => return Error::Sql(err).into_response(),
    };

    Json(pending).into_response()
}

/// Set a build job as "in progress".
async fn put_status(
    AxumState(state): AxumState<Arc<State>>,
    Path((device, md5sum)): Path<(Device, String)>,
    Json(status): Json<Status>,
) -> Response {
    // Only allow marking requests as 'building' in this endpoint.
    match status {
        Status::Building => (),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    }

    // Update request status.
    match state.db.set_status(device, &md5sum, Status::Building).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => err.into_response(),
    }
}

/// Get the status of a build request.
async fn get_status(
    AxumState(state): AxumState<Arc<State>>,
    Path((device, md5sum)): Path<(Device, String)>,
) -> Response {
    // Update request status.
    match state.db.status(device, &md5sum).await {
        Ok(Some(status)) => Json(status).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => err.into_response(),
    }
}

/// Request a new image build.
async fn post_request(
    AxumState(state): AxumState<Arc<State>>,
    Json(body): Json<PostRequestRequest>,
) -> Response {
    // Do some trivial package sanitization.
    //
    // This should not be necessary, but seems prudent to ensure possible future
    // screwups don't put users at risk.
    if let Err(err) = validate_packages(&body.packages) {
        return err.into_response();
    }

    let request = match state.db.add_request(body.device, body.packages).await {
        Ok(status) => status,
        Err(err) => return Error::Sql(err).into_response(),
    };

    Json(request).into_response()
}

/// Upload completed image file.
async fn post_image(
    AxumState(state): AxumState<Arc<State>>,
    headers: HeaderMap,
    Path((device, md5sum, alarm_md5sum)): Path<(Device, String, String)>,
    mut multipart: Multipart,
) -> Response {
    // Validate auth secret.
    if headers.get("authorization").is_none_or(|auth| *auth != state.upload_bearer) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Validate ALARM tarball checksum matches the latest tarball.
    if !state.alarm_checksum_cache.is_latest(&alarm_md5sum).await {
        let latest = state.alarm_checksum_cache.latest().await;
        let msg = format!("MD5 {alarm_md5sum} doesn't match ALARM tarball checksum ({latest:?})");
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }

    // Ensure there is a pending request for this image.
    if let Err(err) = state.db.set_status(device, &md5sum, Status::Writing).await {
        return err.into_response();
    }
    let set_on_drop = SetBuildingOnDrop::new(state, device, md5sum);

    // Get first and only form data field with a file name.
    let mut field = match multipart.next_field().await {
        Ok(Some(field)) => field,
        Ok(None) => return StatusCode::BAD_REQUEST.into_response(),
        Err(err) => return err.status().into_response(),
    };

    // Do some basic validation to simplify debugging.
    if field.file_name().is_none() {
        return (StatusCode::BAD_REQUEST, "multipart/form-data is missing filename")
            .into_response();
    }
    let content_type = field.content_type();
    if content_type != Some("application/octet-stream") {
        let msg = format!("invalid multipart content type: {content_type:?}");
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }

    // Ensure image directory exists.
    if let Err(err) = fs::create_dir_all(IMAGE_DIRECTORY).await {
        error!("Could not create image file directory: {err}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // If the file exists already, but we're unaware of it, nuke it.
    let path = format!("{IMAGE_DIRECTORY}/alarm-{}-{}.img.xz", device, set_on_drop.md5sum);
    let _ = fs::remove_file(&path).await;

    // Open image file handle.
    let mut file = match OpenOptions::new().create_new(true).append(true).open(&path).await {
        Ok(file) => file,
        Err(err) => {
            error!("Could not create image file: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        },
    };

    // Guard to remove the file if write isn't successful.
    let file_drop = RemoveFileOnDrop::new(std::path::Path::new(&path));

    // Write chunked data to our image file.
    loop {
        let mut chunk = match field.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(err) => return (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
        };

        // Ensure enough storage space is available.
        let mut cache_lock = set_on_drop.state().image_cache.write().await;
        if let Err(err) = cache_lock.free_space(chunk.len() as u64).await {
            return err.into_response();
        }

        // Write the data chunk to disk.
        if let Err(err) = file.write_all_buf(&mut chunk).await {
            error!("Failed image write: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }

        drop(cache_lock);
    }

    // Add image to path cache.
    if let Err(err) = set_on_drop.state().image_cache.write().await.add(&path) {
        error!("Unable to add image path to LRU cache: {err}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    file_drop.reclaim();

    // Update status code.
    let ReclaimedSetBuildingOnDrop { state, device, md5sum } = set_on_drop.reclaim();
    if let Err(err) = state.db.set_status(device, &md5sum, Status::Done).await {
        return err.into_response();
    }

    StatusCode::OK.into_response()
}

/// Download built image.
async fn get_image(
    AxumState(state): AxumState<Arc<State>>,
    Path((device, md5sum)): Path<(Device, String)>,
) -> Response {
    // Check for rootfs updates, to invalidate old images.
    if state.alarm_checksum_cache.update_checksum().await.is_some() {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Return 404 if we don't have the image.
    match state.db.status(device, &md5sum).await {
        Ok(Some(Status::Done)) => (),
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => return err.into_response(),
    }

    // Mark image as accessed in LRU cache.
    let filename = format!("alarm-{device}-{md5sum}.img.xz");
    let path = format!("{IMAGE_DIRECTORY}/{filename}");
    state.image_cache.write().await.accessed(&path).await;

    // Get an async read stream over the image's content.
    let file = match File::open(&path).await {
        Ok(file) => ReaderStream::new(file),
        Err(err) => {
            error!("Unable to access completed image file: {err}");

            // Delete requests to allow rebuild which might fix the issue.
            if let Err(err) = state.db.delete(device, &md5sum).await {
                println!("Unable to delete request for {device} {md5sum}: {err}");
            }

            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        },
    };

    // Create our response with headers indicating binary file download.
    let response = Response::builder()
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from_stream(file));

    match response {
        Ok(response) => response,
        Err(err) => {
            error!("Failed to build image GET response: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        },
    }
}

/// Request body for new build requests.
#[derive(Deserialize)]
struct PostRequestRequest {
    packages: Vec<String>,
    device: Device,
}

/// Guard for automatically setting a request's status to 'building' on drop.
struct SetBuildingOnDrop {
    state: Option<Arc<State>>,
    device: Option<Device>,
    md5sum: String,
}

impl SetBuildingOnDrop {
    fn new(state: Arc<State>, device: Device, md5sum: String) -> Self {
        Self { state: Some(state), device: Some(device), md5sum }
    }

    /// Reclaim the state, to avoid calling the drop impl.
    fn reclaim(mut self) -> ReclaimedSetBuildingOnDrop {
        ReclaimedSetBuildingOnDrop {
            state: self.state.take().unwrap(),
            device: mem::take(&mut self.device).unwrap(),
            md5sum: mem::take(&mut self.md5sum),
        }
    }

    /// Get access to the underlying state.
    fn state(&self) -> &Arc<State> {
        self.state.as_ref().unwrap()
    }
}

impl Drop for SetBuildingOnDrop {
    fn drop(&mut self) {
        // Ignore drop if the status was emptied.
        let state = match self.state.take() {
            Some(state) => state,
            None => return,
        };

        // Create tokio background job to reset the status.
        let device = mem::take(&mut self.device).unwrap();
        let md5sum = mem::take(&mut self.md5sum);
        tokio::spawn(async move {
            // SAFETY: We're already in an inconsistent state here, so we just try to
            // forcibly recover to the safest alternative state, which is 'building'.
            if let Err(err) =
                unsafe { state.db.set_status_unchecked(device, &md5sum, Status::Building).await }
            {
                error!("Failed to set {device} {md5sum} to 'building' on drop: {err}");
            }
        });
    }
}

/// Container used when consuming [`SetStatusOnDrop`].
struct ReclaimedSetBuildingOnDrop {
    state: Arc<State>,
    device: Device,
    md5sum: String,
}

struct RemoveFileOnDrop<'a> {
    path: Option<&'a std::path::Path>,
}

impl<'a> RemoveFileOnDrop<'a> {
    fn new(path: &'a std::path::Path) -> Self {
        Self { path: Some(path) }
    }

    /// Reclaim the path, preventing cleanup on drop.
    fn reclaim(mut self) {
        self.path = None;
    }
}

impl<'a> Drop for RemoveFileOnDrop<'a> {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let path = path.to_path_buf();
            tokio::spawn(async move {
                if let Err(err) = fs::remove_file(&path).await {
                    error!("Unable to remove image file after failed write: {err}");
                }
            });
        }
    }
}

/// Validate a build request's package list.
fn validate_packages(packages: &[String]) -> Result<(), Error> {
    for package in packages {
        if package.is_empty()
            || package.starts_with('.')
            || package.starts_with('-')
            || !package
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ['@', '.', '_', '+', '-'].contains(&c))
        {
            return Err(Error::InvalidPackage(package.clone()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_validation() {
        assert!(
            validate_packages(&[
                "valid.".into(),
                "valid-".into(),
                "@valid".into(),
                "va_lid".into(),
                "gnu+linux".into()
            ])
            .is_ok()
        );
        assert!(validate_packages(&["valid".into(), "valid2".into()]).is_ok());

        assert!(validate_packages(&["valid".into(), "-invalid".into(), "valid".into()]).is_err());
        assert!(validate_packages(&[".dotstart".into()]).is_err());
        assert!(validate_packages(&["( ͡° ͜ʖ ͡°)".into()]).is_err());
        assert!(validate_packages(&["\"".into()]).is_err());
        assert!(validate_packages(&["!".into()]).is_err());
        assert!(validate_packages(&["".into()]).is_err());
    }
}
