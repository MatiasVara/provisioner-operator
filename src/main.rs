mod config;
mod controller;
mod error;
mod kubevirt;
mod trustee;

// SPDX-FileCopyrightText: Matias Ezequiel Vara Larsen <mvaralar@redhat.com>
//
// SPDX-License-Identifier: MIT

use std::sync::Arc;

use futures::StreamExt;
use kube::runtime::Controller;
use kube::runtime::watcher::Config as WatchConfig;

use config::Config;
use controller::{Context, VirtualMachineInstance, error_policy, reconcile};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::from_env();
    let client = kube::Client::try_default().await?;
    let http = reqwest::Client::new();

    let vmis: kube::Api<VirtualMachineInstance> =
        kube::Api::namespaced(client.clone(), &config.namespace);

    let ctx = Arc::new(Context {
        client,
        config,
        http,
    });

    Controller::new(vmis, WatchConfig::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            if let Err(e) = result {
                // ObjectNotFound is expected after a VMI is fully deleted
                let msg = format!("{:?}", e);
                if msg.contains("ObjectNotFound") {
                    tracing::debug!("VMI no longer exists (already deleted): {}", msg);
                } else {
                    tracing::error!("Controller error: {:?}", e);
                }
            }
        })
        .await;

    Ok(())
}
