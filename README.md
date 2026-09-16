# provisioner-operator

A Kubernetes operator written in Rust that automates the attestation provisioning flow
for confidential Virtual Machines (CVMs) running on KubeVirt with Intel TDX and AMD SEV-SNP.

## Architecture

The operator bridges two external systems: the **Trustee provisioner plugin** (which holds
VM-specific secrets) and **KubeVirt** (which runs the VM). It watches for VMIs whose
`launchSecurity` section contains an `initDataRef`, fetches the initial configuration data
from Trustee, and creates an **InitData** custom resource that KubeVirt's virt-handler
consumes before starting QEMU.

```
┌──────────────────────────────────────────────────────────────┐
│                      Kubernetes Cluster                       │
│                                                              │
│  ┌──────────────────┐  watch  ┌───────────────────────────┐ │
│  │ provisioner-     │────────>│  VirtualMachineInstance    │ │
│  │ operator         │         │  tdx:                     │ │
│  │                  │         │    initDataRef: "vm-tdx1"  │ │
│  └────────┬─────────┘         └───────────────────────────┘ │
│           │                                                  │
│           │ POST /provision                                  │
│           │ DELETE /provision/{ns}/{name}                     │
│           │                                                  │
│  ┌────────▼─────────┐  create ┌───────────────────────────┐ │
│  │ Trustee           │         │  InitData CR              │ │
│  │ provisioner plugin│ ─ ─ ─ >│  name: "vm-tdx1"          │ │
│  └──────────────────┘         │  mrConfigId: <digest>     │ │
│                               │  oemStrings: [<b64 toml>] │ │
│                               │  ownerRef → VMI           │ │
│                               └───────────────────────────┘ │
└──────────────────────────────────────────────────────────────┘
```

The operator is built with [`kube-rs`](https://github.com/kube-rs/kube) and uses its
controller runtime for watch/reconcile loops and its `Client` for Kubernetes API calls.

## Interaction with KubeVirt

The operator creates an **InitData** custom resource that virt-handler reads
before launching QEMU.

### Reconcile flow

1. A VMI is created with `initDataRef` in its `launchSecurity` (TDX or SEV-SNP).
2. The operator detects the VMI and checks whether an `InitData` CR with the referenced
   name already exists.
3. If not, it contacts Trustee to obtain the VM's provisioning data.
4. It constructs the initdata TOML, computes the measurement digest, and creates the
   `InitData` CR with an `ownerReference` pointing back to the VMI.
5. KubeVirt's virt-handler picks up the `InitData` CR and starts QEMU with the provided
   `mrConfigId` (TDX) or `hostData` (SEV-SNP) and `oemStrings`.

### VMI spec requirements

The VM must declare `initDataRef` inside `launchSecurity`. The referenced name is also
the name of the `InitData` CR that the operator will create.

> **Convention:** use the VMI name as the `initDataRef` value (e.g. `initDataRef: "my-vm"`).
> Since VMI names are unique within a namespace, this guarantees that no two VMIs will
> reference the same `InitData` CR. The operator validates ownership and will reject a
> VMI whose `initDataRef` points to an `InitData` CR already owned by a different VMI.

#### Intel TDX

```yaml
domain:
  launchSecurity:
    tdx:
      initDataRef: "vm-tdx1"
  firmware:
    bootloader:
      efi:
        secureBoot: false
```

#### AMD SEV-SNP

```yaml
domain:
  launchSecurity:
    sevSnp:
      initDataRef: "vm-snp1"
  firmware:
    bootloader:
      efi:
        secureBoot: false
```

The operator handles both platforms in a single binary. For TDX it populates `mrConfigId`
(base64-encoded SHA-384, 48 bytes); for SEV-SNP it populates `hostData` (base64-encoded
SHA-256, 32 bytes). In both cases `oemStrings` carries the base64-encoded initdata TOML
so the guest knows the KBS URL and resource path.

### InitData CR

The operator creates an `InitData` resource like:

```yaml
apiVersion: kubevirt.io/v1
kind: InitData
metadata:
  name: vm-tdx1          # matches initDataRef in the VMI
  namespace: default
  ownerReferences:
  - apiVersion: kubevirt.io/v1
    kind: VirtualMachineInstance
    name: my-vm
    uid: <vmi-uid>
spec:
  mrConfigId: "<base64 SHA-384 digest>"     # TDX only
  # hostData: "<base64 SHA-256 digest>"     # SEV-SNP only
  oemStrings:
  - "<base64-encoded initdata.toml>"
```

The `ownerReference` ensures the `InitData` CR is garbage-collected when the VMI is
deleted.

### Finalizer

The operator adds a finalizer (`provisioner-operator.confidentialcontainers.io/cleanup`)
to each provisioned VMI. This blocks Kubernetes from deleting the VMI until the operator
has notified Trustee to clean up the associated provisioning data. The `InitData` CR
itself is garbage-collected automatically via its `ownerReference`.

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

Response body:

```json
{
  "uuid": "2b48b128-d053-418f-8183-fc6f6d3cf612",
  "resource_path": "default/2b48b128-.../root"
}
```

- **`uuid`**: deterministic identifier for the VM (UUID v5 derived from namespace + name).
- **`resource_path`**: KBS resource path where the LUKS key is stored.

The operator then constructs the initdata TOML locally using `KBS_URL` and `resource_path`,
and derives the following values from it:

- **`mrConfigId`**: base64-encoded SHA-384 digest of initdata.toml (48 bytes). Used for
  Intel TDX — stored in `InitData.spec.mrConfigId`.
- **`hostData`**: base64-encoded SHA-256 digest of initdata.toml (32 bytes).
  Used for AMD SEV-SNP — stored in `InitData.spec.hostData`.
- **`oemStrings`**: base64-encoded initdata.toml, injected as a SMBIOS OEM string (Type 11)
  so the guest knows the KBS URL and the resource path to fetch after attestation.

### Cleanup — `DELETE {KBS_URL}/kbs/v0/provisioner/provision/{namespace}/{name}`

Called when the VMI is deleted. Trustee removes the provisioning record associated with
the VM, revoking access to the KBS resource.

## Running the operator

### Prerequisites

- A Kubernetes cluster with KubeVirt installed (the `InitData` CRD must be registered —
  see the [InitData VEP](https://github.com/kubevirt/enhancements/pull/340)).
- A running Trustee provisioner plugin reachable from the operator.
- `kubectl` configured with access to the cluster (`~/.kube/config` or `KUBECONFIG`).

### Build

The operator handles both TDX and SEV-SNP in a single binary.

```bash
cargo build --release
```

The binary is at `target/release/provisioner-operator`.

### Container image

The `Dockerfile` in the project root builds the operator image:

```bash
podman build -t quay.io/<org>/provisioner-operator:latest .

podman push quay.io/<org>/provisioner-operator:latest
```

### Configuration

All configuration is via environment variables:

| Variable | Default | Description |
|---|---|---|
| `KBS_URL` | `http://127.0.0.1:8080` | Base URL of the KBS server (scheme + host + port). The operator derives the provisioner endpoint (`/kbs/v0/provisioner/provision`) and the health check endpoint (`/healthz`) from this. |
| `WATCH_NAMESPACE` | `default` | Kubernetes namespace to watch for VMIs. |
| `RUST_LOG` | (none) | Log verbosity: `info` for lifecycle events, `debug` for HTTP details. |

### Deploying in-cluster

The manifests in `deploy/operator.yaml` create all the resources needed to run
the operator as a pod: ServiceAccount, ClusterRole, ClusterRoleBinding, and
Deployment.

Before applying, edit `deploy/operator.yaml` to set:

1. **`image:`** — your registry image
2. **`KBS_URL`** — the in-cluster URL of the KBS (e.g. `http://kbs-service.trustee.svc.cluster.local:8080`)
3. **`WATCH_NAMESPACE`** — the namespace where CVMs are created

Then apply:

```bash
oc apply -f deploy/operator.yaml
```

Verify:

```bash
oc get pods -l app=provisioner-operator
oc logs -l app=provisioner-operator -f
```

To remove:

```bash
oc delete -f deploy/operator.yaml
```

### Credentials

The operator uses `kube-rs` for all Kubernetes API calls. It automatically picks up
credentials in this order:

1. **In-cluster**: if running as a pod, uses the mounted ServiceAccount token.
2. **Local**: uses `~/.kube/config` (or the path in `$KUBECONFIG`).

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
