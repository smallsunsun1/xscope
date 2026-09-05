"""Create local Grafana bootstrap credentials once; never rotate an existing Secret."""
import json
import secrets
import subprocess


def main():
    context = subprocess.check_output(["kubectl", "config", "current-context"], text=True).strip()
    if context != "docker-desktop":
        raise SystemExit("Local Docker Desktop only; current context was not changed.")
    command = ["kubectl", "--context", context, "-n", "xscope-system"]
    existing = subprocess.check_output(command + ["get", "secret", "xscope-grafana", "--ignore-not-found", "-o", "name"], text=True)
    if existing.strip():
        print("Existing Grafana credentials retained.")
        return
    secret = {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "xscope-grafana"},
              "type": "Opaque", "stringData": {"admin-user": "admin", "admin-password": secrets.token_urlsafe(32),
                                               "secret-key": secrets.token_urlsafe(48)}}
    # Do not pass secrets through command-line arguments, files or console output.
    subprocess.run(command + ["create", "-f", "-"], input=json.dumps(secret), text=True, check=True)
    print("Created Grafana credentials in Secret xscope-grafana; no plaintext credentials were written.")


if __name__ == "__main__":
    main()
