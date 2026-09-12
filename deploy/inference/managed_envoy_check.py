"""Validate the generated profile with the pinned Envoy binary; no cluster writes."""
from pathlib import Path
import subprocess
import sys
import yaml
from python.runfiles import runfiles


def main():
    resolver = runfiles.Create()
    config = Path(resolver.Rlocation(sys.argv[1])).resolve()
    objects = yaml.safe_load_all(Path(resolver.Rlocation(sys.argv[2])).read_text())
    deployment = next(o for o in objects if o and o.get("kind") == "Deployment")
    image = next(c["image"] for c in deployment["spec"]["template"]["spec"]["containers"] if c["name"] == "envoy")
    subprocess.run(["docker", "image", "inspect", image], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    result = subprocess.run(["docker", "run", "--rm", "--network", "none", "--cpus", "0.5", "--memory", "256m",
        "--entrypoint", "/usr/local/bin/envoy", "--mount", f"type=bind,src={config},dst=/tmp/managed.json,readonly", image,
        "--mode", "validate", "-c", "/tmp/managed.json", "--log-level", "error"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=45)
    if result.returncode:
        raise RuntimeError("synthetic Envoy profile validation failed: " + result.stderr[-2000:])
    print("PASS pinned Envoy validates generated managed profile with network disabled; no live deployment")


if __name__ == "__main__":
    main()
