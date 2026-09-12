"""Generate local bootstrap secrets once; never rotate an existing identity/DB."""
import base64
import json
import secrets
import subprocess

from observability_cluster import KUBE, local_only


def captured(*args, payload=None):
    result = subprocess.run(KUBE + list(args), input=json.dumps(payload) if payload else None,
                            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if result.returncode:
        raise RuntimeError("Kubernetes secret operation failed; output suppressed")
    return result.stdout


def create_if_missing(name, data):
    existing = captured("get", "secret", name, "--ignore-not-found", "-o", "name")
    if existing.strip():
        # Remove historical client-side apply's duplicate plaintext/base64 copy.
        captured("annotate", "secret", name, "kubectl.kubernetes.io/last-applied-configuration-")
        print(f"Existing Secret {name} retained; no credentials rotated")
        return
    if name == "xscope-platform-secrets" and captured("get", "pvc", "data-postgres-0", "--ignore-not-found", "-o", "name").strip():
        raise RuntimeError("Database PVC exists but platform Secret is missing; restore the Secret instead of generating a new DB password")
    captured("create", "-f", "-", payload={"apiVersion": "v1", "kind": "Secret",
        "metadata": {"name": name, "namespace": "xscope-system"}, "type": "Opaque", "stringData": data})
    print(f"Created Secret {name}; values were not printed or written to disk")


def main():
    local_only()
    password = secrets.token_urlsafe(32)
    create_if_missing("xscope-platform-secrets", {
        "postgres-password": password,
        "platform-database-url": f"postgres://xscope:{password}@postgres:5432/keycloak?sslmode=disable",
        "keycloak-admin-password": secrets.token_urlsafe(32),
        "bootstrap-user-password": secrets.token_urlsafe(32),
        "oauth-client-secret": secrets.token_urlsafe(32),
        "oauth-cookie-secret": base64.urlsafe_b64encode(secrets.token_bytes(32)).decode(),
        "internal-token": secrets.token_urlsafe(32),
    })
    create_if_missing("xscope-gateway-keys", {"keys.json": json.dumps([{
        "id": "key-local", "tenant_id": "tenant-local", "project_id": "project-local",
        "secret": secrets.token_urlsafe(32),
    }])})
    create_if_missing("xscope-backend-runtime", {"XSCOPE_EVENT_WORKER_ENABLED": "true"})


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.SubprocessError, OSError) as error:
        raise SystemExit(str(error) if isinstance(error, RuntimeError) else "Secret initialization failed; raw output suppressed") from None
