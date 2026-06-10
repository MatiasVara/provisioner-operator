// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

use crate::error::Error;

// Injects mrConfigId and oemStrings into a VMI via the KubeVirt tdx/injectInitdata subresource.
//
// Instead of calling the Kubernetes API server through an oc proxy (an external HTTP proxy
// running on localhost), we use the kube::Client directly. The client already holds valid
// credentials (kubeconfig locally, or a ServiceAccount token when running in-cluster), so
// no proxy is needed.
//
// The PUT request is built as a raw http::Request because kube-rs does not have a typed
// binding for KubeVirt subresources. The client routes the request to the correct API server
// and handles authentication automatically.
pub async fn inject_initdata(
    client: &kube::Client,
    namespace: &str,
    name: &str,
    mr_config_id: &str,
    oem_strings: &[String],
) -> Result<(), Error> {
    let payload = serde_json::json!({
        "mrConfigId": mr_config_id,
        "oemStrings": oem_strings,
    });

    let req = http::Request::builder()
        .method(http::Method::PUT)
        .uri(format!(
            "/apis/subresources.kubevirt.io/v1/namespaces/{}/virtualmachineinstances/{}/tdx/injectInitdata",
            namespace, name
        ))
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_vec(&payload)
                .map_err(|e| Error::ProvisioningError(e.to_string()))?,
        )
        .map_err(|e| Error::ProvisioningError(e.to_string()))?;

    // kube::Client::request_text returns Err if the server responds with a non-2xx status,
    // so we do not need to inspect the status code manually.
    let body = client.request_text(req).await?;
    tracing::debug!("inject_initdata response body: {}", body);

    Ok(())
}

// Unpauses a VirtualMachineInstance so the guest starts executing after attestation is complete.
//
// In this cluster's KubeVirt version, the unpause subresource is registered under
// virtualmachineinstances (VMI), not virtualmachines (VM). This can be verified with:
//   kubectl get --raw /apis/subresources.kubevirt.io/v1 | python3 -m json.tool | grep unpause
//
// Previously this called `virtctl unpause` as an external subprocess, which required
// virtctl to be installed on the host. Using kube::Client directly removes that dependency
// and works both locally (via kubeconfig) and in-cluster (via ServiceAccount token).
pub async fn unpause(client: &kube::Client, namespace: &str, name: &str) -> Result<(), Error> {
    let req = http::Request::builder()
        .method(http::Method::PUT)
        .uri(format!(
            "/apis/subresources.kubevirt.io/v1/namespaces/{}/virtualmachineinstances/{}/unpause",
            namespace, name
        ))
        .header("Content-Type", "application/json")
        .body(b"{}".to_vec())
        .map_err(|e| Error::ProvisioningError(e.to_string()))?;

    let body = client.request_text(req).await?;
    tracing::debug!("unpause response body: {}", body);

    Ok(())
}
