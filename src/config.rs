// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

// Operator configuration loaded from environment variables.
// All fields have defaults so the operator can run out of the box in development.
#[derive(Debug, Clone)]
pub struct Config {
    // Base URL of the Trustee provisioner plugin.
    // Example: "http://trustee-plugin.trustee.svc.cluster.local:8080/kbs/v0/provisioner"
    pub trustee_url: String,

    // Kubernetes namespace the operator watches for VMIs.
    // Set WATCH_NAMESPACE to an empty string to watch all namespaces (not yet supported).
    pub namespace: String,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            trustee_url: std::env::var("TRUSTEE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080/kbs/v0/provisioner".to_string()),
            namespace: std::env::var("WATCH_NAMESPACE").unwrap_or_else(|_| "default".to_string()),
        }
    }
}
