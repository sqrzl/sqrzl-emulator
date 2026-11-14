from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest


@pytest.mark.skipif(shutil.which("dotnet") is None, reason="The pinned .NET SDK is not installed")
def test_gcs_dotnet_default_gzip_workflow(sqrzl_server, tmp_path, record_property):
    sqrzl_server.require_provider("gcs")
    project = Path(__file__).parent / "dotnet" / "Gcs" / "Gcs.csproj"
    artifacts = tmp_path / "dotnet-artifacts"
    build = subprocess.run(
        ["dotnet", "build", str(project), "--configuration", "Release", "--artifacts-path", str(artifacts), "--property:RestoreLockedMode=true", "--nologo"],
        cwd=project.parent, capture_output=True, text=True, timeout=120,
    )
    assert build.returncode == 0, build.stdout + build.stderr
    assembly = next((artifacts / "bin" / "Gcs").rglob("Gcs.dll"))
    environment = os.environ.copy()
    environment["GCS_ENDPOINT"] = sqrzl_server.api_url.rstrip("/") + "/"
    environment["GCS_BEARER_TOKEN"] = sqrzl_server.gcs_hmac_secret if sqrzl_server.enforce_auth else ""
    result = subprocess.run(["dotnet", str(assembly)], cwd=project.parent, env=environment, capture_output=True, text=True, timeout=60)
    assert result.returncode == 0, result.stdout + result.stderr
    receipt = json.loads(result.stdout.strip().splitlines()[-1])
    assert receipt["version"] == "5.0.0"
    assert receipt["gzip_default"] is True
    assert "gzip-bucket-create" in receipt["assertions"]
    receipt["dependency_lock_sha256"] = hashlib.sha256((project.parent / "packages.lock.json").read_bytes()).hexdigest()
    receipt["runtime_sdk"] = subprocess.check_output(["dotnet", "--version"], cwd=project.parent, text=True).strip()
    assert receipt["runtime_sdk"] == "11.0.100"
    record_property("dotnet_gcs_receipt", receipt)
