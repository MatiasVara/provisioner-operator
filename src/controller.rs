// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{ObjectMeta, PatchParams, PostParams};
use kube::runtime::controller::Action;
use kube::{Api, CustomResource};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::time::Duration;

use crate::config::Config;
use crate::error::Error;
use crate::trustee;

// --- VMI types (partial, only the fields the operator needs) ---

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
    pub tdx: Option<Tdx>,
    #[serde(rename = "sevSnp")]
    pub sev_snp: Option<SevSnp>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct Tdx {
    #[serde(rename = "initDataRef", default)]
    pub init_data_ref: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct SevSnp {
    #[serde(rename = "initDataRef", default)]
    pub init_data_ref: String,
}

// --- InitData CRD types ---

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "kubevirt.io",
    version = "v1",
    kind = "InitData",
    namespaced,
    status = "InitDataStatus"
)]
pub struct InitDataSpec {
    #[serde(rename = "mrConfigId", skip_serializing_if = "Option::is_none")]
    pub mr_config_id: Option<String>,
    #[serde(rename = "hostData", skip_serializing_if = "Option::is_none")]
    pub host_data: Option<String>,
    #[serde(rename = "oemStrings")]
    pub oem_strings: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct InitDataStatus {
    #[serde(default)]
    pub conditions: Option<Vec<serde_json::Value>>,
}

// --- Operator context ---

pub struct Context {
    pub client: kube::Client,
    pub config: Config,
    pub http: reqwest::Client,
}

const FINALIZER: &str = "provisioner-operator.confidentialcontainers.io/cleanup";

/// TEE platform detected from the VMI spec.
enum TeePlatform {
    Tdx,
    Snp,
}

/// Extracts the `initDataRef` value and TEE platform from a VMI.
/// Returns `None` if neither TDX nor SNP has `initDataRef` set.
fn extract_init_data_ref(vmi: &VirtualMachineInstance) -> Option<(String, TeePlatform)> {
    let ls = vmi.spec.domain.as_ref()?.launch_security.as_ref()?;

    if let Some(tdx) = &ls.tdx
        && !tdx.init_data_ref.is_empty()
    {
        return Some((tdx.init_data_ref.clone(), TeePlatform::Tdx));
    }

    if let Some(snp) = &ls.sev_snp
        && !snp.init_data_ref.is_empty()
    {
        return Some((snp.init_data_ref.clone(), TeePlatform::Snp));
    }

    None
}

// --- Reconcile loop ---

pub async fn reconcile(
    vmi: Arc<VirtualMachineInstance>,
    ctx: Arc<Context>,
) -> Result<Action, Error> {
    let name = vmi.metadata.name.as_deref().unwrap_or("");
    let namespace = vmi.metadata.namespace.as_deref().unwrap_or("default");

    if vmi.metadata.deletion_timestamp.is_some() {
        return handle_deletion(&vmi, &ctx).await;
    }

    let Some((init_data_ref, platform)) = extract_init_data_ref(&vmi) else {
        return Ok(Action::requeue(Duration::from_secs(300)));
    };

    let initdata_api: Api<InitData> = Api::namespaced(ctx.client.clone(), namespace);

    match initdata_api.get(&init_data_ref).await {
        Ok(existing) => {
            let owned_by_us = existing
                .metadata
                .owner_references
                .as_ref()
                .and_then(|refs| refs.first())
                .is_some_and(|r| {
                    r.kind == "VirtualMachineInstance"
                        && r.name == name
                        && r.uid == vmi.metadata.uid.as_deref().unwrap_or("")
                });

            if owned_by_us {
                tracing::debug!(
                    "InitData {} already exists for VMI {}/{}",
                    init_data_ref,
                    namespace,
                    name
                );
                return Ok(Action::requeue(Duration::from_secs(300)));
            }

            tracing::error!(
                "InitData {} already exists but is owned by a different VMI. \
                 VMI {}/{} cannot reuse it — use a unique initDataRef name.",
                init_data_ref,
                namespace,
                name
            );
            return Err(Error::Provisioning(format!(
                "InitData {} is already owned by another VMI",
                init_data_ref
            )));
        }
        Err(kube::Error::Api(err)) if err.code == 404 => {
            // InitData does not exist yet — proceed to provision
        }
        Err(e) => return Err(e.into()),
    }

    provision_vmi(name, namespace, &init_data_ref, &platform, &vmi, &ctx).await?;

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

// --- Finalizer management ---

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
        let api: Api<VirtualMachineInstance> = Api::namespaced(
            client.clone(),
            vmi.metadata.namespace.as_deref().unwrap_or("default"),
        );
        let patch = serde_json::json!({
            "metadata": { "finalizers": [FINALIZER] }
        });
        api.patch(
            vmi.metadata.name.as_deref().unwrap_or(""),
            &PatchParams::apply("provisioner-operator"),
            &kube::api::Patch::Merge(&patch),
        )
        .await?;
    }
    Ok(())
}

// --- Provisioning ---

async fn provision_vmi(
    name: &str,
    namespace: &str,
    init_data_ref: &str,
    platform: &TeePlatform,
    vmi: &VirtualMachineInstance,
    ctx: &Arc<Context>,
) -> Result<(), Error> {
    ensure_finalizer(vmi, &ctx.client).await?;

    // Re-check after finalizer patch to avoid duplicate provisioning from
    // the reconcile event triggered by the finalizer patch itself.
    let initdata_api: Api<InitData> = Api::namespaced(ctx.client.clone(), namespace);
    if initdata_api.get(init_data_ref).await.is_ok() {
        tracing::info!(
            "InitData {} already exists (re-check), skipping",
            init_data_ref
        );
        return Ok(());
    }

    tracing::info!("Contacting Trustee for {}/{}", namespace, name);

    trustee::health_check(&ctx.http, &ctx.config.health_url()).await?;
    let data = trustee::provision(
        &ctx.http,
        &ctx.config.provisioner_url(),
        &ctx.config.kbs_url,
        name,
        namespace,
    )
    .await?;

    tracing::info!(
        "Creating InitData {} for VMI {}/{}",
        init_data_ref,
        namespace,
        name
    );

    let (mr_config_id, host_data) = match platform {
        TeePlatform::Tdx => (Some(data.mr_config_id), None),
        TeePlatform::Snp => (None, Some(data.hostdata)),
    };

    let vmi_uid = vmi.metadata.uid.as_deref().unwrap_or("").to_string();

    let initdata = InitData {
        metadata: ObjectMeta {
            name: Some(init_data_ref.to_string()),
            namespace: Some(namespace.to_string()),
            owner_references: Some(vec![OwnerReference {
                api_version: "kubevirt.io/v1".to_string(),
                kind: "VirtualMachineInstance".to_string(),
                name: name.to_string(),
                uid: vmi_uid,
                ..Default::default()
            }]),
            ..Default::default()
        },
        spec: InitDataSpec {
            mr_config_id,
            host_data,
            oem_strings: data.oem_strings,
        },
        status: None,
    };

    initdata_api
        .create(&PostParams::default(), &initdata)
        .await?;

    tracing::info!(
        "InitData {} created for VMI {}/{}",
        init_data_ref,
        namespace,
        name
    );

    Ok(())
}

// --- Deletion / cleanup ---

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
        ctx.config.provisioner_url(),
        namespace,
        name
    );
    let response = ctx.http.delete(&url).send().await?;
    tracing::info!("Trustee cleanup response: status={}", response.status());

    // The InitData CR is garbage-collected by Kubernetes via ownerReference.

    tracing::info!("Removing finalizer from VMI {}/{}", namespace, name);
    let api: Api<VirtualMachineInstance> = Api::namespaced(ctx.client.clone(), namespace);
    let patch = serde_json::json!({
        "metadata": { "finalizers": [] }
    });
    api.patch(
        name,
        &PatchParams::apply("provisioner-operator"),
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
