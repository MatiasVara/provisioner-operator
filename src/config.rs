// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

// Operator configuration loaded from environment variables.
// All fields have defaults so the operator can run out of the box in development.
#[derive(Debug, Clone)]
pub struct Config {
    // Base URL of the KBS server (scheme + host + port).
    // Example: "http://trustee-plugin.trustee.svc.cluster.local:8080"
    pub kbs_url: String,

    // Kubernetes namespace the operator watches for VMIs.
    // Set WATCH_NAMESPACE to an empty string to watch all namespaces (not yet supported).
    pub namespace: String,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            kbs_url: std::env::var("KBS_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string()),
            namespace: std::env::var("WATCH_NAMESPACE").unwrap_or_else(|_| "default".to_string()),
        }
    }

    // Full URL for the provisioner plugin endpoint.
    pub fn provisioner_url(&self) -> String {
        format!("{}/kbs/v0/provisioner", self.kbs_url)
    }

    // Full URL for the KBS health check endpoint.
    pub fn health_url(&self) -> String {
        format!("{}/healthz", self.kbs_url)
    }
}
