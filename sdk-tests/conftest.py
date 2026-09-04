from __future__ import annotations

import base64
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
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_ACCESS_KEY = "sqrzl-access"
DEFAULT_SECRET_KEY = base64.b64encode(b"sqrzl-secret").decode("ascii")
AZURE_ACCOUNT = "devstoreaccount1"
ACS_ACCESS_KEY = base64.b64encode(b"shared-secret").decode("ascii")
TWILIO_ACCOUNT_SID = "AC00000000000000000000000000000001"
TWILIO_AUTH_TOKEN = "sqrzl-twilio-token"


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

    def require_provider(self, provider: str) -> None:
        if provider not in self.enabled_providers:
            pytest.skip(f"{provider} SDK tests disabled by SQRZL_SDK_PROVIDERS")

    def bucket_name(self, prefix: str) -> str:
        return f"{prefix}-{uuid.uuid4().hex[:16]}".lower()


def _providers_from_env() -> frozenset[str]:
    raw = os.getenv(
        "SQRZL_SDK_PROVIDERS",
        "s3,azure,gcs,oci,email,twilio,sns,aws-sms-voice-v2",
    )
    providers = {provider.strip().lower() for provider in raw.split(",") if provider.strip()}
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


def _wait_for_health(api_url: str, process: subprocess.Popen[str] | None = None) -> None:
    deadline = time.monotonic() + 30
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            output = process.stdout.read() if process.stdout is not None else ""
            raise RuntimeError(f"SQRZL exited before /healthz became ready:\n{output}")
        try:
            with urllib.request.urlopen(f"{api_url}/healthz", timeout=1) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError) as exc:
            last_error = exc
        time.sleep(0.1)
    raise RuntimeError(f"SQRZL /healthz did not become ready at {api_url}: {last_error}")


def _binary_path() -> Path:
    configured = os.getenv("SQRZL_BINARY")
    if configured:
        return Path(configured)
    return REPO_ROOT / "target" / "debug" / "sqrzl-emulator"


def _ensure_binary() -> Path:
    binary = _binary_path()
    if binary.exists():
        return binary
    subprocess.run(
        ["cargo", "build", "--bin", "sqrzl-emulator"],
        cwd=REPO_ROOT,
        check=True,
    )
    return binary


@pytest.fixture(scope="session")
def sqrzl_server() -> SqrzlSettings:
    api_url = os.getenv("SQRZL_API_URL")
    enabled_providers = _providers_from_env()
    enforce_auth = os.getenv("SQRZL_SDK_ENFORCE_AUTH") == "1"
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
        oci_public_key_path.write_bytes(
            oci_key.public_key().public_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PublicFormat.SubjectPublicKeyInfo,
            )
        )
        env["OCI_TENANCY_OCID"] = "ocid1.tenancy.oc1..sqrzl"
        env["OCI_USER_OCID"] = "ocid1.user.oc1..sqrzl"
        env["OCI_KEY_FINGERPRINT"] = (
            "00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff"
        )
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

    process = subprocess.Popen(
        [str(binary)],
        cwd=REPO_ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
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
    )

    try:
        _wait_for_health(settings.api_url, process)
        yield settings
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        shutil.rmtree(runtime_dir, ignore_errors=True)
