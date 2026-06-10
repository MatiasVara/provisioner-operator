// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

use serde::Deserialize;

use crate::error::Error;

#[derive(Deserialize)]
pub struct TrusteeResponse {
    pub mrconfigid: String,
    pub oemstring: String,
}

// Internal representation mapping Trustee field names to KubeVirt subresource names
pub struct ProvisionedData {
    pub mr_config_id: String,
    pub oem_strings: Vec<String>,
}

pub async fn provision(
    client: &reqwest::Client,
    trustee_url: &str,
    name: &str,
    namespace: &str,
) -> Result<ProvisionedData, Error> {
    let response = client
        .post(format!("{}/provision", trustee_url))
        .json(&serde_json::json!({
            "vm_name": name,
            "namespace": namespace,
        }))
        .send()
        .await?;

    let status = response.status();
    let body = response.text().await?;

    tracing::debug!("Trustee response status={} body={}", status, body);

    if !status.is_success() {
        return Err(Error::ProvisioningError(format!(
            "Trustee returned HTTP {}: {}",
            status, body
        )));
    }

    let parsed = serde_json::from_str::<TrusteeResponse>(&body).map_err(|e| {
        Error::ProvisioningError(format!(
            "Failed to parse Trustee response: {} — body was: {:?}",
            e, body
        ))
    })?;

    Ok(ProvisionedData {
        mr_config_id: parsed.mrconfigid,
        oem_strings: vec![parsed.oemstring],
    })
}
