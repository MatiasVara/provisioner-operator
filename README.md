# provisioner-operator

A Kubernetes operator written in Rust that automates the attestation provisioning flow
for confidential Virtual Machines (CVMs) running on KubeVirt with Intel TDX and AMD SEV.

## Architecture

The operator bridges two external systems: the **Trustee provisioner plugin** (which holds
VM-specific secrets) and **KubeVirt** (which runs the VM). It watches for VMIs that declare
they need attestation, fetches their initial configuration data from Trustee, injects it into
the VMI before QEMU starts, and then unpauses the VM so the guest can boot. This is the
behavior when a YAML description requires some external operator to inject measurements before it boots up.

```
┌─────────────────────────────────────────────────────────────┐
│                     Kubernetes Cluster                       │
│                                                             │
│  ┌──────────────────┐       ┌──────────────────────────┐   │
│  │ provisioner-     │ watch │  VirtualMachineInstance   │   │
│  │ operator         │──────>│  (phase: Scheduled)       │   │
│  │                  │       │  tdx.attestation: {}      │   │
│  │                  │       │  tdx.mrConfigId: ""       │   │
│  └────────┬─────────┘       └──────────────────────────┘   │
│           │                                                  │
│           │ POST /provision          PUT tdx/injectInitdata  │
│           │ DELETE /provision/{ns}/{name}   PUT unpause      │
│           │                                                  │
│  ┌────────▼─────────┐       ┌──────────────────────────┐   │
│  │ Trustee           │       │  KubeVirt subresource     │   │
│  │ provisioner plugin│       │  API (virt-api)           │   │
│  └──────────────────┘       └──────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

The operator is built with [`kube-rs`](https://github.com/kube-rs/kube) and uses its
controller runtime for watch/reconcile loops and its `Client` for Kubernetes API calls.

## Interaction with KubeVirt

The operator relies on two KubeVirt subresource endpoints under
`/apis/subresources.kubevirt.io/v1/`:

### 1. Inject initdata — `PUT .../virtualmachineinstances/{name}/tdx/injectInitdata`

Injects the TDX configuration data into the VMI before QEMU starts. The request body is:

```json
{
  "mrConfigId": "<base64-encoded 48-byte TDX measurement config ID>",
  "oemStrings": ["<path to secret in KBS, e.g. kbs:///default/uuid/root>"]
}
```

This endpoint is only accepted while the VMI is in the `Scheduled` phase (virt-handler
has claimed the VMI but QEMU has not started yet) and the VMI has `tdx.attestation: {}`
set in its spec. After injection, virt-handler starts QEMU with the provided values.

### 2. Unpause — `PUT .../virtualmachineinstances/{name}/unpause`

The VMI is created with `startStrategy: Paused` so that QEMU boots in paused state,
giving the operator a window to inject the attestation data. Once injection is complete
and the VMI reaches the `Running` phase (QEMU started but execution is paused), the
operator calls this endpoint to resume execution.

> **Note:** the `unpause` subresource is registered under `virtualmachineinstances` in
> this KubeVirt build. You can verify it with:
> ```bash
> kubectl get --raw /apis/subresources.kubevirt.io/v1 | python3 -m json.tool | grep unpause
> ```

### VMI spec requirements

The VM must be created with the following fields in `spec.template.spec`.

#### Intel TDX

```yaml
startStrategy: Paused
domain:
  launchSecurity:
    tdx:
      attestation: {}   # signals to the operator that provisioning is needed
  firmware:
    bootloader:
      efi:
        secureBoot: false
```

The operator checks for `tdx.attestation` being present and `tdx.mrConfigId` being empty
to decide whether provisioning is needed. After injection, `mrConfigId` holds a
base64-encoded 48-byte value that extends the TDX measurement (MRCONFIGID register).

#### AMD SEV-SNP

> **Note:** SEV-SNP support is not yet implemented in the operator. The KubeVirt API
> fields described below are defined but the reconcile loop currently only handles TDX.

```yaml
startStrategy: Paused
domain:
  launchSecurity:
    sevSnp:
      attestation: {}   # analogous to TDX — signals provisioning is needed
  firmware:
    bootloader:
      efi:
        secureBoot: false
```

For SEV-SNP the equivalent of `mrConfigId` is `hostData`, a base64-encoded 32-byte value
that is passed as the `HOST_DATA` measurement during VM launch. The `oemStrings` field
works identically to TDX and carries the KBS resource path for the guest to fetch its
secret after attestation.

### Finalizer

The operator adds a finalizer (`provisioner-operator.confidentialcontainers.io/cleanup`)
to each provisioned VMI. This blocks Kubernetes from deleting the VMI until the operator
has notified Trustee to clean up the associated provisioning data.

## Interaction with Trustee provisioner plugin

The Trustee provisioner plugin is an HTTP service that manages per-VM secrets. The operator
communicates with it via two REST calls:

### Health check — `GET {KBS_URL}/healthz`

Before provisioning, the operator calls the KBS health endpoint to verify the service
is alive. If the health check fails, provisioning is skipped and retried later.

### Provision — `POST {KBS_URL}/kbs/v0/provisioner/provision`

Called when a new VMI needs configuration data. Request body:

```json
{
  "vm_name": "tdx1",
  "namespace": "default"
}
```

Response body (fields used by the operator):

```json
{
  "uuid": "2b48b128-d053-418f-8183-fc6f6d3cf612",
  "mrconfigid": "<base64-encoded 48-byte value>",
  "oemstring": "kbs+provisioner:///default/2b48b128-.../root",
  "resource_path": "default/2b48b128-.../root"
}
```

The `mrconfigid` becomes `tdx.mrConfigId` in the VMI spec (used to extend the TDX
measurement). The `oemstring` is injected as an SMBIOS OEM string (Type 11), which the
guest reads at boot to locate its secret in KBS.

### Cleanup — `DELETE {KBS_URL}/kbs/v0/provisioner/provision/{namespace}/{name}`

Called when the VMI is deleted. Trustee removes the provisioning record associated with
the VM, revoking access to the KBS resource.

## Running the operator

### Prerequisites

- A Kubernetes cluster with KubeVirt installed (the custom `tdx/injectInitdata` subresource
  must be present — it is not part of upstream KubeVirt yet).
- A running Trustee provisioner plugin reachable from the operator.
- `kubectl` configured with access to the cluster (`~/.kube/config` or `KUBECONFIG`).
- Rust toolchain (edition 2024, see `Cargo.toml`).

### Build

```bash
cargo build --release
```

The binary is at `target/release/provisioner-operator`.

### Configuration

All configuration is via environment variables:

| Variable | Default | Description |
|---|---|---|
| `KBS_URL` | `http://127.0.0.1:8080` | Base URL of the KBS server (scheme + host + port). The operator derives the provisioner endpoint (`/kbs/v0/provisioner/provision`) and the health check endpoint (`/healthz`) from this. |
| `WATCH_NAMESPACE` | `default` | Kubernetes namespace to watch for VMIs. |

Example for a KBS running at `10.44.34.144:8080`:

```bash
export KBS_URL=http://10.44.34.144:8080
export WATCH_NAMESPACE=default
./target/release/provisioner-operator
```

### Credentials

The operator uses `kube-rs` for all Kubernetes API calls. It automatically picks up
credentials in this order:

1. **In-cluster**: if running as a pod, uses the mounted ServiceAccount token.
2. **Local**: uses `~/.kube/config` (or the path in `$KUBECONFIG`).

No `oc proxy` or external tooling is needed.

### Running locally (out-of-cluster)

```bash
# Point to the cluster where KubeVirt and Trustee are running
export KUBECONFIG=~/.kube/config
export KBS_URL=http://<kbs-host>:<port>
export WATCH_NAMESPACE=default
export RUST_LOG=info   # set to debug for verbose HTTP logs

cargo run
```

### Logging

The operator uses `tracing` with `tracing-subscriber`. Set the `RUST_LOG` environment
variable to control verbosity:

```bash
RUST_LOG=debug   # show all HTTP request/response details
RUST_LOG=info    # show provisioning lifecycle events (recommended)
RUST_LOG=warn    # show only errors and warnings
```

## Work in progress

### Authentication between the operator and Trustee (not yet implemented)

Currently the operator communicates with the Trustee provisioner plugin over plain HTTP
with no authentication. Any process that can reach the Trustee endpoint can request
provisioning data for any VM. Two options are being considered:

---

#### Option 1 — Bearer token (admin token)

KBS already has an admin token mechanism. The same
approach can be applied to the provisioner plugin endpoint.

**How it would work:**

1. A shared secret token is generated when Trustee is deployed and stored as a Kubernetes
   Secret.
2. The operator mounts that Secret as an environment variable (`TRUSTEE_ADMIN_TOKEN`).
3. Every request from the operator to Trustee includes the header:
   ```
   Authorization: Bearer <token>
   ```
4. The provisioner plugin validates the header and rejects requests without a valid token.

**Trade-offs:**
- Simple to implement; well-understood pattern.
- The token is a static shared secret — if it leaks (e.g. in logs or env dumps) an
  attacker gains full access to the provisioner API.
- Token rotation requires restarting both Trustee and the operator.

---

#### Option 2 — Mutual TLS (mTLS)

Both the operator and Trustee present X.509 certificates signed by a shared internal CA.
The connection is rejected if either side cannot prove its identity.

**How it would work:**

1. An internal CA is created (e.g. via `cert-manager` in the cluster).
2. Trustee gets a server certificate; the operator gets a client certificate, both signed
   by the internal CA.
3. Certificates are stored in Kubernetes Secrets and mounted into the respective pods.
4. The operator configures `reqwest` with the client certificate and the CA bundle;
   Trustee requires a valid client certificate on every connection.
5. `cert-manager` can automate certificate rotation without restarting the workloads.

**Trade-offs:**
- Stronger guarantee: authentication is cryptographic and bilateral — the operator proves
  who it is, not just that it knows a secret.
- More operational complexity: requires a CA, certificate issuance, and mount configuration.
- Natural fit for the confidential containers ecosystem, where mTLS is already used for
  inter-component communication.

---

## Source layout

```
src/
├── main.rs         # entry point: sets up the kube-rs controller and wires dependencies
├── controller.rs   # reconcile loop: watches VMIs, drives the provisioning state machine
├── trustee.rs      # HTTP client for the Trustee provisioner plugin
├── kubevirt.rs     # HTTP client for KubeVirt subresource API (inject, unpause)
├── config.rs       # configuration from environment variables
└── error.rs        # custom error types (thiserror)
```
