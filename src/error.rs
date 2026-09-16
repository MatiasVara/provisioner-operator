// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes error: {0}")]
    Kube(#[from] kube::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Provisioning failed: {0}")]
    Provisioning(String),
}
