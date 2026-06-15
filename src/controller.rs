// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

use kube::CustomResource;
use kube::runtime::controller::Action;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::time::Duration;

use crate::config::Config;
use crate::error::Error;
use crate::kubevirt;
use crate::trustee;

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "kubevirt.io",
    version = "v1",
    kind = "VirtualMachineInstance",
    namespaced,
    status = "VirtualMachineInstanceStatus"
)]
pub struct VirtualMachineInstanceSpec {
    pub domain: Option<DomainSpec>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct VirtualMachineInstanceStatus {
    pub phase: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct DomainSpec {
    #[serde(rename = "launchSecurity")]
    pub launch_security: Option<LaunchSecurity>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct LaunchSecurity {
    pub tdx: Option<TDX>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct TDX {
    pub attestation: Option<serde_json::Value>,
    #[serde(rename = "mrConfigId", default)]
    pub mr_config_id: String,
}

pub struct Context {
    pub client: kube::Client,
    pub config: Config,
    pub http: reqwest::Client,
}

const FINALIZER: &str = "provisioner-operator.confidentialcontainers.io/cleanup";

pub async fn reconcile(
    vmi: Arc<VirtualMachineInstance>,
    ctx: Arc<Context>,
) -> Result<Action, Error> {
    let name = vmi.metadata.name.as_deref().unwrap_or("");
    let namespace = vmi.metadata.namespace.as_deref().unwrap_or("default");

    if vmi.metadata.deletion_timestamp.is_some() {
        return handle_deletion(&vmi, &ctx).await;
    }

    let phase = vmi
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .unwrap_or("");

    let needs_provisioning = phase == "Scheduled"
        && vmi
            .spec
            .domain
            .as_ref()
            .and_then(|d| d.launch_security.as_ref())
            .and_then(|ls| ls.tdx.as_ref())
            .map(|tdx| tdx.attestation.is_some() && tdx.mr_config_id.is_empty())
            .unwrap_or(false);

    if needs_provisioning {
        provision_vmi(name, namespace, &vmi, &ctx).await?;
    }

    Ok(Action::requeue(Duration::from_secs(300)))
}

pub fn error_policy(
    _vmi: Arc<VirtualMachineInstance>,
    error: &Error,
    _ctx: Arc<Context>,
) -> Action {
    tracing::error!("Error reconciling VMI: {}", error);
    Action::requeue(Duration::from_secs(30))
}

async fn ensure_finalizer(
    vmi: &VirtualMachineInstance,
    client: &kube::Client,
) -> Result<(), Error> {
    let has_finalizer = vmi
        .metadata
        .finalizers
        .as_ref()
        .map(|f| f.contains(&FINALIZER.to_string()))
        .unwrap_or(false);

    if !has_finalizer {
        let api: kube::Api<VirtualMachineInstance> = kube::Api::namespaced(
            client.clone(),
            vmi.metadata.namespace.as_deref().unwrap_or("default"),
        );
        let patch = serde_json::json!({
            "metadata": { "finalizers": [FINALIZER] }
        });
        api.patch(
            vmi.metadata.name.as_deref().unwrap_or(""),
            &kube::api::PatchParams::apply("tdx-operator"),
            &kube::api::Patch::Merge(&patch),
        )
        .await?;
    }
    Ok(())
}

async fn provision_vmi(
    name: &str,
    namespace: &str,
    vmi: &VirtualMachineInstance,
    ctx: &Arc<Context>,
) -> Result<(), Error> {
    ensure_finalizer(vmi, &ctx.client).await?;

    // Re-read VMI from API to avoid stale cache triggering duplicate provisioning.
    // Each patch (finalizer, injectInitdata) triggers a new reconcile event, which
    // may arrive with an outdated cached copy that still shows mrConfigId as empty.
    let api: kube::Api<VirtualMachineInstance> =
        kube::Api::namespaced(ctx.client.clone(), namespace);
    let current = api.get(name).await?;
    let already_provisioned = current
        .spec
        .domain
        .as_ref()
        .and_then(|d| d.launch_security.as_ref())
        .and_then(|ls| ls.tdx.as_ref())
        .map(|tdx| !tdx.mr_config_id.is_empty())
        .unwrap_or(false);

    if already_provisioned {
        tracing::info!("VMI {}/{} already provisioned, skipping", namespace, name);
        return Ok(());
    }

    tracing::info!("Contacting Trustee for {}/{}", namespace, name);

    trustee::health_check(&ctx.http, &ctx.config.health_url()).await?;
    let data = trustee::provision(&ctx.http, &ctx.config.provisioner_url(), name, namespace).await?;

    tracing::info!("Injecting initdata for {}/{}", namespace, name);
    kubevirt::inject_initdata(
        &ctx.client,
        namespace,
        name,
        &data.mr_config_id,
        &data.oem_strings,
    )
    .await?;

    // Wait for virt-handler to detect mrConfigId and start QEMU (VMI → Running)
    // before calling unpause. Without this, unpause fails with "VMI is not running".
    wait_for_running_phase(name, namespace, &ctx.client).await?;

    tracing::info!("Unpausing VMI {}/{}", namespace, name);
    kubevirt::unpause(&ctx.client, namespace, name).await?;

    Ok(())
}

async fn wait_for_running_phase(
    name: &str,
    namespace: &str,
    client: &kube::Client,
) -> Result<(), Error> {
    let api: kube::Api<VirtualMachineInstance> = kube::Api::namespaced(client.clone(), namespace);

    for attempt in 1..=30 {
        let vmi = api.get(name).await?;
        let phase = vmi
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("");

        tracing::debug!(
            "Waiting for Running phase, attempt {}/30, current phase={}",
            attempt,
            phase
        );

        if phase == "Running" {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    Err(Error::ProvisioningError(format!(
        "VMI {}/{} did not reach Running phase within 60 seconds",
        namespace, name
    )))
}

async fn handle_deletion(
    vmi: &VirtualMachineInstance,
    ctx: &Arc<Context>,
) -> Result<Action, Error> {
    let name = vmi.metadata.name.as_deref().unwrap_or("");
    let namespace = vmi.metadata.namespace.as_deref().unwrap_or("default");

    let has_our_finalizer = vmi
        .metadata
        .finalizers
        .as_ref()
        .map(|f| f.iter().any(|s| s == FINALIZER))
        .unwrap_or(false);

    if !has_our_finalizer {
        tracing::info!(
            "VMI {}/{} has no finalizer, skipping cleanup",
            namespace,
            name
        );
        return Ok(Action::await_change());
    }

    tracing::info!("VMI {}/{} deleted, notifying Trustee", namespace, name);
    let url = format!(
        "{}/provision/{}/{}",
        ctx.config.provisioner_url(), namespace, name
    );
    let response = ctx.http.delete(&url).send().await?;
    tracing::info!("Trustee cleanup response: status={}", response.status());

    tracing::info!("Removing finalizer from VMI {}/{}", namespace, name);
    let api: kube::Api<VirtualMachineInstance> =
        kube::Api::namespaced(ctx.client.clone(), namespace);
    let patch = serde_json::json!({
        "metadata": { "finalizers": [] }
    });
    api.patch(
        name,
        &kube::api::PatchParams::apply("provisioner-operator"),
        &kube::api::Patch::Merge(&patch),
    )
    .await?;

    tracing::info!(
        "Finalizer removed, VMI {}/{} can be deleted",
        namespace,
        name
    );
    Ok(Action::await_change())
}
