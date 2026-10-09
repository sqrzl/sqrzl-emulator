from __future__ import annotations

import base64
import importlib.metadata
import hashlib
import json
import inspect
import re
import os
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_ACCESS_KEY = "sqrzl-access"
DEFAULT_SECRET_KEY = base64.b64encode(b"sqrzl-secret").decode("ascii")
AZURE_ACCOUNT = "devstoreaccount1"
ACS_ACCESS_KEY = base64.b64encode(b"shared-secret").decode("ascii")
TWILIO_ACCOUNT_SID = "AC00000000000000000000000000000001"
TWILIO_AUTH_TOKEN = "sqrzl-twilio-token"
SENDGRID_API_KEY = "SG.sqrzl-sdk-qualification"


class SqrzlRuntime:
    """One owned child process; restart retains ports, credentials and storage."""

    def __init__(
        self, binary: Path, env: dict[str, str], runtime_dir: Path, api_url: str
    ):
        self.binary = binary
        self.env = env
        self.runtime_dir = runtime_dir
        self.log_path = runtime_dir / "emulator.log"
        self.api_url = api_url
        self.process = None
        self.events = []
        self._log_file = None
        with binary.open("rb") as stream:
            self.binary_sha256 = hashlib.file_digest(stream, "sha256").hexdigest()

    @property
    def process_pid(self) -> int | None:
        return (
            self.process.pid if self.process and self.process.poll() is None else None
        )

    def start(self) -> int:
        if self.process_pid is not None:
            raise RuntimeError("SQRZL child is already running")
        with self.binary.open("rb") as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != self.binary_sha256:
                raise RuntimeError("SQRZL binary changed during a qualification run")
        self._log_file = self.log_path.open("ab")
        self.process = subprocess.Popen(
            [str(self.binary)],
            cwd=REPO_ROOT,
            env=self.env,
            stdout=self._log_file,
            stderr=subprocess.STDOUT,
        )
        try:
            _wait_for_health(self.api_url, self.process)
        except Exception:
            self.stop(kill=True)
            raise RuntimeError(
                f"SQRZL startup failed; log tail:\n{self.log_path.read_text(errors='replace')[-8000:]}"
            )
        self.events.append(
            {
                "kind": "start",
                "pid": self.process.pid,
                "binary_sha256": self.binary_sha256,
            }
        )
        return self.process.pid

    def stop(self, kill: bool = False) -> int | None:
        if self.process is None:
            return None
        pid = self.process.pid
        timed_out = False
        if self.process.poll() is None:
            self.process.kill() if kill else self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
                timed_out = True
        self.events.append(
            {
                "kind": (
                    "stop-timeout"
                    if timed_out
                    else "abrupt-stop" if kill else "normal-stop"
                ),
                "pid": pid,
                "exit_code": self.process.returncode,
            }
        )
        if self._log_file is not None:
            self._log_file.close()
            self._log_file = None
        if timed_out:
            raise RuntimeError("SQRZL stop timed out; child was killed and reaped")
        return pid

    def restart(self, kill: bool = False) -> int:
        previous_pid = self.stop(kill=kill)
        pid = self.start()
        assert pid != previous_pid, "restart must create an independent child process"
        return pid


@dataclass(frozen=True)
class SqrzlSettings:
    api_url: str
    ui_url: str
    access_key_id: str
    secret_access_key: str
    azure_account: str
    azure_account_key: str
    gcs_hmac_access_id: str
    gcs_hmac_secret: str
    oci_tenancy_ocid: str
    oci_user_ocid: str
    oci_key_fingerprint: str
    oci_private_key_path: Path | None
    smtp_port: int
    storage_dir: Path | None
    enabled_providers: frozenset[str]
    enforce_auth: bool
    messaging_auth: bool = False
    sendgrid_api_key: str = SENDGRID_API_KEY
    runtime: SqrzlRuntime | None = None

    def require_provider(self, provider: str) -> None:
        if provider not in self.enabled_providers:
            pytest.skip(f"{provider} SDK tests disabled by SQRZL_SDK_PROVIDERS")

    def bucket_name(self, prefix: str) -> str:
        return f"{prefix}-{uuid.uuid4().hex[:16]}".lower()

    @property
    def process_pid(self) -> int | None:
        return self.runtime.process_pid if self.runtime else None

    @property
    def runtime_dir(self) -> Path | None:
        return self.runtime.runtime_dir if self.runtime else None

    @property
    def log_path(self) -> Path | None:
        return self.runtime.log_path if self.runtime else None

    def require_process(self) -> SqrzlRuntime:
        if self.runtime is None:
            pytest.skip(
                "process restart requires a managed local child; remote endpoint is smoke evidence"
            )
        return self.runtime

    def restart(self, kill: bool = False) -> int:
        return self.require_process().restart(kill=kill)

    def require_messaging_auth(self) -> None:
        if not self.messaging_auth:
            pytest.skip(
                "configured messaging authentication requires SQRZL_SDK_MESSAGING_AUTH=1"
            )


def _providers_from_env() -> frozenset[str]:
    raw = os.getenv(
        "SQRZL_SDK_PROVIDERS",
        "s3,azure,gcs,oci,email,twilio,sns,aws-sms-voice-v2",
    )
    providers = {
        provider.strip().lower() for provider in raw.split(",") if provider.strip()
    }
    aliases = {
        "s3-family": "s3",
        "azure-blob": "azure",
        "oci-object": "oci",
        "smtp": "smtp",
        "sendgrid": "sendgrid",
        "ses": "ses",
        "acs": "acs",
        "sms-voice": "aws-sms-voice-v2",
        "pinpoint-sms-voice-v2": "aws-sms-voice-v2",
    }
    normalized = {aliases.get(provider, provider) for provider in providers}
    if "email" in normalized:
        normalized.remove("email")
        normalized.update({"smtp", "sendgrid", "ses", "acs"})
    return frozenset(normalized)


def _reserve_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _wait_for_health(
    api_url: str, process: subprocess.Popen[str] | None = None
) -> None:
    deadline = time.monotonic() + 30
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            raise RuntimeError(
                f"SQRZL exited before /healthz became ready: {process.returncode}"
            )
        try:
            with urllib.request.urlopen(f"{api_url}/healthz", timeout=1) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError) as exc:
            last_error = exc
        time.sleep(0.1)
    raise RuntimeError(
        f"SQRZL /healthz did not become ready at {api_url}: {last_error}"
    )


def _binary_path() -> Path:
    configured = os.getenv("SQRZL_BINARY")
    if configured:
        return Path(configured)
    return REPO_ROOT / "target" / "debug" / "sqrzl-emulator"


def _ensure_binary() -> Path:
    binary = _binary_path()
    if os.getenv("SQRZL_BINARY") and binary.exists():
        return binary
    subprocess.run(
        ["cargo", "build", "--locked", "--bin", "sqrzl-emulator"],
        cwd=REPO_ROOT,
        check=True,
    )
    return binary


@pytest.fixture(scope="session")
def sqrzl_server() -> SqrzlSettings:
    api_url = os.getenv("SQRZL_API_URL")
    enabled_providers = _providers_from_env()
    messaging_auth = os.getenv("SQRZL_SDK_MESSAGING_AUTH") == "1"
    enforce_auth = os.getenv("SQRZL_SDK_ENFORCE_AUTH") == "1" or messaging_auth
    if api_url:
        smtp_port = int(os.getenv("SQRZL_SMTP_PORT", "2525"))
        yield SqrzlSettings(
            api_url=api_url.rstrip("/"),
            ui_url=os.getenv("SQRZL_UI_URL", "").rstrip("/"),
            access_key_id=os.getenv("SQRZL_ACCESS_KEY_ID", DEFAULT_ACCESS_KEY),
            secret_access_key=os.getenv("SQRZL_SECRET_ACCESS_KEY", DEFAULT_SECRET_KEY),
            azure_account=os.getenv("AZURE_ACCOUNT", AZURE_ACCOUNT),
            azure_account_key=os.getenv("AZURE_ACCOUNT_KEY", DEFAULT_SECRET_KEY),
            gcs_hmac_access_id=os.getenv("GCS_HMAC_ACCESS_ID", DEFAULT_ACCESS_KEY),
            gcs_hmac_secret=os.getenv("GCS_HMAC_SECRET", DEFAULT_SECRET_KEY),
            oci_tenancy_ocid=os.getenv("OCI_TENANCY_OCID", "ocid1.tenancy.oc1..sqrzl"),
            oci_user_ocid=os.getenv("OCI_USER_OCID", "ocid1.user.oc1..sqrzl"),
            oci_key_fingerprint=os.getenv(
                "OCI_KEY_FINGERPRINT",
                "00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff",
            ),
            oci_private_key_path=(
                Path(os.environ["OCI_PRIVATE_KEY_PATH"])
                if "OCI_PRIVATE_KEY_PATH" in os.environ
                else None
            ),
            smtp_port=smtp_port,
            storage_dir=None,
            enabled_providers=enabled_providers,
            enforce_auth=enforce_auth,
            messaging_auth=messaging_auth,
            sendgrid_api_key=os.getenv("SQRZL_SENDGRID_API_KEY", SENDGRID_API_KEY),
        )
        return

    api_port = _reserve_port()
    smtp_port = _reserve_port()
    ui_port = _reserve_port()
    runtime_dir = Path(tempfile.mkdtemp(prefix="sqrzl-sdk-runtime-"))
    storage_dir = runtime_dir / "blobs"
    storage_dir.mkdir()
    binary = _ensure_binary()
    env = os.environ.copy()
    # Never inherit unrelated developer/provider credentials into qualification.
    for name in [
        "SQRZL_ACCESS_KEY_ID",
        "SQRZL_SECRET_ACCESS_KEY",
        "AZURE_ACCOUNT",
        "AZURE_ACCOUNT_KEY",
        "GCS_HMAC_ACCESS_ID",
        "GCS_HMAC_SECRET",
        "OCI_TENANCY_OCID",
        "OCI_USER_OCID",
        "OCI_KEY_FINGERPRINT",
        "OCI_PUBLIC_KEY_PATH",
        "SQRZL_ACS_CONNECTION_STRING",
        "SQRZL_TWILIO_ACCOUNT_SID",
        "SQRZL_TWILIO_AUTH_TOKEN",
        "SQRZL_SENDGRID_API_KEY",
    ]:
        env.pop(name, None)
    env.update(
        {
            "SQRZL_API_PORT": str(api_port),
            "SQRZL_SMTP_PORT": str(smtp_port),
            "SQRZL_UI_PORT": str(ui_port),
            "SQRZL_BLOBS_PATH": str(storage_dir),
            "SQRZL_ADMIN_AUTH_DISABLED": "true",
            "RUST_LOG": env.get("RUST_LOG", "sqrzl_emulator=info"),
        }
    )
    if "acs" in enabled_providers:
        env["SQRZL_ACS_CONNECTION_STRING"] = (
            f"endpoint=http://127.0.0.1:{api_port};accesskey={ACS_ACCESS_KEY}"
        )
    if "twilio" in enabled_providers:
        env["SQRZL_TWILIO_ACCOUNT_SID"] = TWILIO_ACCOUNT_SID
        env["SQRZL_TWILIO_AUTH_TOKEN"] = TWILIO_AUTH_TOKEN
    if "sendgrid" in enabled_providers:
        env["SQRZL_SENDGRID_API_KEY"] = SENDGRID_API_KEY
    if enforce_auth:
        env["SQRZL_ACCESS_KEY_ID"] = DEFAULT_ACCESS_KEY
        env["SQRZL_SECRET_ACCESS_KEY"] = DEFAULT_SECRET_KEY
        env["AZURE_ACCOUNT"] = AZURE_ACCOUNT
        env["AZURE_ACCOUNT_KEY"] = DEFAULT_SECRET_KEY
        env["GCS_HMAC_ACCESS_ID"] = DEFAULT_ACCESS_KEY
        env["GCS_HMAC_SECRET"] = DEFAULT_SECRET_KEY
        serialization = pytest.importorskip(
            "cryptography.hazmat.primitives.serialization"
        )
        rsa = pytest.importorskip("cryptography.hazmat.primitives.asymmetric.rsa")
        oci_private_key_path = runtime_dir / "oci_api_key.pem"
        oci_public_key_path = runtime_dir / "oci_api_key_public.pem"
        oci_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        oci_private_key_path.write_bytes(
            oci_key.private_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PrivateFormat.TraditionalOpenSSL,
                encryption_algorithm=serialization.NoEncryption(),
            )
        )
        oci_private_key_path.chmod(0o600)
        oci_public_key_path.write_bytes(
            oci_key.public_key().public_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PublicFormat.SubjectPublicKeyInfo,
            )
        )
        env["OCI_TENANCY_OCID"] = "ocid1.tenancy.oc1..sqrzl"
        env["OCI_USER_OCID"] = "ocid1.user.oc1..sqrzl"
        env["OCI_KEY_FINGERPRINT"] = "00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff"
        env["OCI_PUBLIC_KEY_PATH"] = str(oci_public_key_path)
    else:
        oci_private_key_path = None
        env.pop("SQRZL_ACCESS_KEY_ID", None)
        env.pop("SQRZL_SECRET_ACCESS_KEY", None)
        for name in [
            "AZURE_ACCOUNT",
            "AZURE_ACCOUNT_KEY",
            "GCS_HMAC_ACCESS_ID",
            "GCS_HMAC_SECRET",
            "OCI_TENANCY_OCID",
            "OCI_USER_OCID",
            "OCI_KEY_FINGERPRINT",
            "OCI_PUBLIC_KEY_PATH",
        ]:
            env.pop(name, None)

    runtime = SqrzlRuntime(binary, env, runtime_dir, f"http://127.0.0.1:{api_port}")
    settings = SqrzlSettings(
        api_url=f"http://127.0.0.1:{api_port}",
        ui_url=f"http://127.0.0.1:{ui_port}",
        access_key_id=DEFAULT_ACCESS_KEY,
        secret_access_key=DEFAULT_SECRET_KEY,
        azure_account=AZURE_ACCOUNT,
        azure_account_key=DEFAULT_SECRET_KEY,
        gcs_hmac_access_id=DEFAULT_ACCESS_KEY,
        gcs_hmac_secret=DEFAULT_SECRET_KEY,
        oci_tenancy_ocid="ocid1.tenancy.oc1..sqrzl",
        oci_user_ocid="ocid1.user.oc1..sqrzl",
        oci_key_fingerprint="00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff",
        oci_private_key_path=oci_private_key_path,
        smtp_port=smtp_port,
        storage_dir=storage_dir,
        enabled_providers=enabled_providers,
        enforce_auth=enforce_auth,
        messaging_auth=messaging_auth,
        runtime=runtime,
    )

    try:
        runtime.start()
        yield settings
    finally:
        try:
            runtime.stop()
        finally:
            _PROCESS_EVENTS.extend(runtime.events)
            shutil.rmtree(runtime_dir, ignore_errors=True)


_RESULTS = []
_PROCESS_EVENTS = []


def pytest_sessionstart(session):
    if os.getenv("SQRZL_SDK_ALLOW_UPGRADE") == "1":
        return
    baseline = json.loads(
        (REPO_ROOT / "sdk-tests" / "qualification-baseline.json").read_text()
    )
    for package, expected in baseline["dependency_versions"].items():
        try:
            actual = importlib.metadata.version(package)
        except importlib.metadata.PackageNotFoundError:
            actual = "missing"
        if actual != expected:
            raise pytest.UsageError(
                f"SDK qualification requires {package}=={expected}, found {actual}; install requirements.lock or explicitly use SQRZL_SDK_ALLOW_UPGRADE=1"
            )


def pytest_collection_modifyitems(items):
    manifest = json.loads(
        (REPO_ROOT / "sdk-tests" / "acceptance-manifest.json").read_text()
    )
    missing = [
        item.nodeid
        for item in items
        if item.nodeid.split("[")[0] not in manifest["tests"]
    ]
    if missing:
        raise pytest.UsageError(
            f"SDK tests must declare their acceptance scope: {missing}"
        )


def _resolved_api_versions():
    # Resolve version selectors from the installed official SDKs, including upgrade candidates.
    from botocore.session import Session
    from azure.storage.blob._shared.constants import X_MS_VERSION
    from azure.communication.email import EmailClient
    from azure.communication.sms import SmsClient
    from google.cloud.storage._http import Connection
    from oci.object_storage import ObjectStorageClient

    aws = Session()
    connection = f"endpoint=http://127.0.0.1:1;accesskey={ACS_ACCESS_KEY}"
    email = EmailClient.from_connection_string(connection)
    sms = SmsClient.from_connection_string(connection)
    try:
        oci_source = Path(inspect.getfile(ObjectStorageClient)).read_text()
        oci_version = re.search(r"API Version: (\d+)", oci_source)
        return {
            **{
                name: aws.get_service_model(name).api_version
                for name in ["s3", "sns", "sesv2", "pinpoint-sms-voice-v2"]
            },
            "azure-blob": X_MS_VERSION,
            "acs-email": email._config.api_version,
            "acs-sms": sms._sms_service_client._config.api_version,
            "gcs-json": Connection.API_VERSION,
            "oci-object-storage": oci_version.group(1) if oci_version else "unknown",
            "gcs-xml-signing": "V2 HMAC",
            "sendgrid": "v3",
            "twilio": "2010-04-01",
            "smtp": "RFC 5321 supported command subset",
        }
    finally:
        email.close()
        sms._sms_service_client.close()


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    report = (yield).get_result()
    if report.when == "teardown" and report.failed:
        for result in _RESULTS:
            if result["test"] == item.nodeid:
                result["outcome"] = "failed"
                result["reason"] = f"teardown: {report.longrepr}"
        return
    if report.when == "call" or (report.when == "setup" and report.outcome != "passed"):
        _RESULTS.append(
            {
                "test": item.nodeid,
                "outcome": report.outcome,
                "duration_seconds": report.duration,
                "properties": dict(report.user_properties),
                "property_entries": report.user_properties,
                "reason": str(report.longrepr) if report.outcome != "passed" else None,
            }
        )


def pytest_sessionfinish(session, exitstatus):
    lane = os.getenv("SQRZL_SDK_LANE", "functional")
    path = Path(
        os.getenv(
            "SQRZL_SDK_EVIDENCE",
            str(REPO_ROOT / "target" / "sdk-evidence" / f"{lane}.json"),
        )
    )
    manifest_path = REPO_ROOT / "sdk-tests" / "acceptance-manifest.json"
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}
    for result in _RESULTS:
        result["acceptance"] = manifest.get("tests", {}).get(
            result["test"].split("[")[0], {"scope": "unmapped"}
        )
    source = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, text=True
    ).strip()
    dirty = bool(
        subprocess.check_output(
            ["git", "status", "--porcelain"], cwd=REPO_ROOT, text=True
        ).strip()
    )
    versions = {}
    for package in [
        "pytest",
        "boto3",
        "botocore",
        "azure-storage-blob",
        "google-cloud-storage",
        "google-auth",
        "oci",
        "sendgrid",
        "azure-communication-email",
        "azure-communication-sms",
        "twilio",
    ]:
        try:
            versions[package] = importlib.metadata.version(package)
        except importlib.metadata.PackageNotFoundError:
            versions[package] = None
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "source_commit": source,
                "dirty_worktree": dirty,
                "recorded_at": datetime.now(timezone.utc).isoformat(),
                "lane": lane,
                "python": __import__("sys").version,
                "workflow_sha": os.getenv("GITHUB_SHA"),
                "workflow_run_id": os.getenv("GITHUB_RUN_ID"),
                "sdk_versions": versions,
                "api_versions": _resolved_api_versions(),
                "baseline_api_versions": manifest.get("api_versions", {}),
                "dependency_versions": {
                    d.metadata["Name"]: d.version
                    for d in importlib.metadata.distributions()
                },
                "baseline_lock_sha256": hashlib.sha256(
                    (REPO_ROOT / "sdk-tests" / "requirements.lock").read_bytes()
                ).hexdigest(),
                "upgrade_candidate": os.getenv("SQRZL_SDK_ALLOW_UPGRADE") == "1",
                "enabled_providers": sorted(_providers_from_env()),
                "messaging_auth_lane": os.getenv("SQRZL_SDK_MESSAGING_AUTH") == "1",
                "gcs_json_auth": (
                    "disabled"
                    if "gcs" not in _providers_from_env()
                    else (
                        "local-bearer-convenience"
                        if os.getenv("SQRZL_SDK_ENFORCE_AUTH") == "1"
                        or os.getenv("SQRZL_SDK_MESSAGING_AUTH") == "1"
                        else "anonymous-convenience"
                    )
                ),
                "native_auth_tests": [
                    r["test"]
                    for r in _RESULTS
                    if r["outcome"] == "passed"
                    and "native-auth" in r["acceptance"].get("checks", [])
                ],
                "process_events": _PROCESS_EVENTS,
                "remote_endpoint": bool(os.getenv("SQRZL_API_URL")),
                "collection_only": session.config.option.collectonly,
                "exit_status": int(exitstatus),
                "results": _RESULTS,
            },
            indent=2,
        )
        + "\n"
    )
