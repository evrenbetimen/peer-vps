//! Guest image management for the Host view: what is installed, what can be
//! downloaded, background downloads with progress the UI polls, and imports
//! of the provider's own ISOs and disks (a Windows installer, for example).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use peervps_core::Error;
use peervps_core::virtualization::images::{self, CATALOG, CatalogImage, LocalImage};
use serde::Serialize;
use tauri::State;
use tokio::sync::Mutex;

use crate::AppState;
use crate::commands::CmdError;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    done: u64,
    total: Option<u64>,
    error: Option<String>,
    /// A local file being copied in rather than a download.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    import: bool,
}

#[derive(Debug)]
pub struct ImageStore {
    dir: PathBuf,
    http: reqwest::Client,
    downloads: Arc<Mutex<HashMap<String, Download>>>,
}

impl ImageStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, http: reqwest::Client::new(), downloads: Arc::default() }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Images {
    dir: PathBuf,
    installed: Vec<LocalImage>,
    catalog: &'static [CatalogImage],
    downloads: HashMap<String, Download>,
}

#[tauri::command]
pub async fn list_images(state: State<'_, AppState>) -> Result<Images, CmdError> {
    let s = &state.images;
    Ok(Images {
        dir: s.dir.clone(),
        installed: images::list(&s.dir)?,
        catalog: CATALOG,
        downloads: s.downloads.lock().await.clone(),
    })
}

/// Start downloading a catalog image; progress shows up in `list_images`.
#[tauri::command]
pub async fn pull_image(state: State<'_, AppState>, name: String) -> Result<(), CmdError> {
    if CatalogImage::get(&name).is_none() {
        return Err(Error::NotFound(format!("no catalog image {name:?}")).into());
    }
    let s = &state.images;
    {
        let mut downloads = s.downloads.lock().await;
        if downloads.get(&name).is_some_and(|d| d.error.is_none()) {
            return Ok(()); // already running
        }
        downloads.insert(name.clone(), Download::default());
    }
    let (dir, http, downloads) = (s.dir.clone(), s.http.clone(), s.downloads.clone());
    tauri::async_runtime::spawn(async move {
        let progress = {
            let (downloads, name) = (downloads.clone(), name.clone());
            move |done, total| {
                // try_lock: never stall the download on a UI poll.
                if let Ok(mut d) = downloads.try_lock() {
                    d.insert(name.clone(), Download { done, total, ..Download::default() });
                }
            }
        };
        let result = images::pull(&http, &dir, &name, progress).await;
        let mut d = downloads.lock().await;
        match result {
            Ok(_) => {
                d.remove(&name);
            }
            Err(e) => {
                tracing::warn!(image = %name, error = %e, "image download failed");
                d.insert(name, Download { error: Some(e.to_string()), ..Download::default() });
            }
        }
    });
    Ok(())
}

/// Ask for an ISO or qcow2 file and copy it into the image directory in the
/// background. Returns the new image's name, or `None` if the picker was cancelled.
#[tauri::command]
pub async fn import_image(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<Option<String>, CmdError> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Add an installer ISO or a qcow2 disk")
        .add_filter("ISO or qcow2", &["iso", "qcow2"])
        .pick_file(move |f| {
            let _ = tx.send(f);
        });
    let Some(file) = rx.await.ok().flatten() else { return Ok(None) };
    let src = file.into_path().map_err(|e| Error::Invalid(e.to_string()))?;
    let name = images::name_from_file(&src);
    images::validate_name(&name)?;
    let s = &state.images;
    {
        let mut downloads = s.downloads.lock().await;
        if downloads.get(&name).is_some_and(|d| d.error.is_none()) {
            return Ok(Some(name)); // already copying
        }
        let total = std::fs::metadata(&src).ok().map(|m| m.len());
        downloads.insert(name.clone(), Download { total, import: true, ..Download::default() });
    }
    let (dir, downloads, task_name) = (s.dir.clone(), s.downloads.clone(), name.clone());
    tauri::async_runtime::spawn(async move {
        let result = images::import(&dir, &src, Some(&task_name)).await;
        let mut d = downloads.lock().await;
        match result {
            Ok(_) => {
                d.remove(&task_name);
            }
            Err(e) => {
                tracing::warn!(image = %task_name, error = %e, "image import failed");
                d.insert(task_name, Download { error: Some(e.to_string()), import: true, ..Download::default() });
            }
        }
    });
    Ok(Some(name))
}
