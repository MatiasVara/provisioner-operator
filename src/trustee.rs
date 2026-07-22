// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

use serde::Deserialize;

use crate::error::Error;

#[derive(Deserialize)]
pub struct TrusteeResponse {
    pub uuid: String,
    pub resource_path: String,
}

// Internal representation mapping Trustee field names to KubeVirt subresource names
pub struct ProvisionedData {
    pub mr_config_id: String,
    pub hostdata: String,
    pub oem_strings: Vec<String>,
}

// Verify that KBS is healthy before attempting to provision.
pub async fn health_check(client: &reqwest::Client, health_url: &str) -> Result<(), Error> {
    let response = client.get(health_url).send().await?;
    let status = response.status();
    tracing::debug!("Trustee health check: status={}", status);

    if !status.is_success() {
        return Err(Error::ProvisioningError(format!(
            "Trustee health check failed: HTTP {}",
            status
        )));
    }
    Ok(())
}

pub async fn provision(
    client: &reqwest::Client,
    provisioner_url: &str,
    kbs_url: &str,
    name: &str,
    namespace: &str,
) -> Result<ProvisionedData, Error> {
    let response = client
        .post(format!("{}/provision", provisioner_url))
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

    let initdata_toml = format!(
        "algorithm = \"sha384\"\n\
         version = \"0.1.0\"\n\
         \n\
         [data]\n\
         \"trustee.kbs.url\" = \"{}\"\n\
         \"trustee.kbs.resource\" = \"kbs+provisioner:///{}\"\n",
        kbs_url, parsed.resource_path
    );

    use base64::{Engine, engine::general_purpose::STANDARD as B64};
    use sha2::{Digest, Sha384};

    let oemstring = B64.encode(initdata_toml.as_bytes());
    let digest = Sha384::digest(initdata_toml.as_bytes());
    let mrconfigid = B64.encode(digest);
    let hostdata = B64.encode(&digest[..32]);

    Ok(ProvisionedData {
        mr_config_id: mrconfigid,
        hostdata,
        oem_strings: vec![oemstring],
    })
}
