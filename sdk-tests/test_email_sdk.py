from __future__ import annotations

import os
import base64
import smtplib
import time
import urllib.error
import urllib.parse
import urllib.request
import json
from email.message import EmailMessage
from pathlib import Path
from typing import Any

import pytest


def _mailbox_subject(prefix: str) -> tuple[str, str]:
    suffix = Path("/tmp").name if Path("/tmp").exists() else "msg"
    mailbox = f"{prefix}-{suffix}-{os.urandom(4).hex()}@example.com"
    subject = f"Email emulator SDK {prefix} {mailbox}"
    return mailbox, subject


def _admin_json_request(
    server,
    path: str,
    method: str = "GET",
    body: dict[str, object] | None = None,
) -> dict[str, Any]:
    if not server.ui_url:
        pytest.skip("ui_url is not configured for SDK fixture")

    if not hasattr(_admin_json_request, "_opener"):
        opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor())
        _admin_json_request._opener = opener
    else:
        opener = _admin_json_request._opener

    ui_url = server.ui_url.rstrip("/")

    login_url = f"{ui_url}/admin/v1/auth/login"
    login_request = urllib.request.Request(
        login_url,
        method="POST",
        headers={"content-type": "application/json"},
        data=(
            f'{{"username":"{server.access_key_id}","password":"{server.secret_access_key}"}}'
        ).encode(),
    )
    try:
        opener.open(login_request)
    except urllib.error.HTTPError:
        # If login is unavailable for this fixture mode, continue and rely on unauthenticated
        # admin endpoints if the server is configured with ADMIN_AUTH_DISABLED=true.
        pass

    headers = {"accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode()
        headers["content-type"] = "application/json"
    else:
        data = None

    request = urllib.request.Request(
        f"{ui_url}{path}",
        method=method,
        headers=headers,
        data=data,
    )
    with opener.open(request) as response:
        payload = response.read()
    return json.loads(payload)


def _list_mailbox_messages(server, mailbox: str) -> list[dict[str, Any]]:
    encoded = urllib.parse.quote(mailbox, safe="")
    response = _admin_json_request(
        server,
        f"/admin/v1/mailboxes/{encoded}/messages",
        "GET",
    )
    return response.get("items", [])


def _get_mailbox_message(server, mailbox: str, message_id: str) -> dict[str, Any]:
    encoded = urllib.parse.quote(mailbox, safe="")
    encoded_message = urllib.parse.quote(message_id, safe="")
    return _admin_json_request(
        server,
        f"/admin/v1/mailboxes/{encoded}/messages/{encoded_message}",
        "GET",
    )


def _wait_for_messages(server, mailbox: str, minimum: int = 1) -> list[dict[str, Any]]:
    deadline = time.time() + 8
    while time.time() < deadline:
        messages = _list_mailbox_messages(server, mailbox)
        if len(messages) >= minimum:
            return messages
        time.sleep(0.2)
    return messages


def test_smtp_sdk_sends_message(sqrzl_server):
    sqrzl_server.require_provider("smtp")
    mailbox, subject = _mailbox_subject("smtp")

    message = EmailMessage()
    message["From"] = "sender@example.com"
    message["To"] = mailbox
    message["Subject"] = subject
    message.set_content(f"SDK SMTP body for {subject}")

    with smtplib.SMTP("127.0.0.1", sqrzl_server.smtp_port, timeout=5) as client:
        client.send_message(message)

    messages = _wait_for_messages(sqrzl_server, mailbox)
    assert messages, f"no messages found for mailbox {mailbox}"
    detail = _get_mailbox_message(sqrzl_server, mailbox, messages[0]["message_id"])
    assert detail["subject"] == subject


def test_sendgrid_sdk_send(sqrzl_server):
    sqrzl_server.require_provider("sendgrid")
    sendgrid = pytest.importorskip("sendgrid")
    from sendgrid.helpers.mail import Mail

    mailbox, subject = _mailbox_subject("sendgrid")
    api_key = os.getenv("SQRZL_SENDGRID_API_KEY", "SG.dummy")
    os.environ["SQRZL_SENDGRID_API_KEY"] = api_key
    client = sendgrid.SendGridAPIClient(api_key)
    client.client.host = sqrzl_server.api_url

    body = Mail(
        from_email="sender@example.com",
        to_emails=mailbox,
        subject=subject,
        plain_text_content=f"SDK SendGrid body for {subject}",
        html_content=f"<p>SDK SendGrid body for {subject}</p>",
    )
    response = client.send(body)

    assert int(response.status_code) in {200, 201, 202}
    header = {name.lower(): value for name, value in response.headers.items()}
    assert header.get("x-message-id")

    messages = _wait_for_messages(sqrzl_server, mailbox)
    assert messages, f"no messages found for mailbox {mailbox}"
    detail = _get_mailbox_message(sqrzl_server, mailbox, messages[0]["message_id"])
    assert detail["subject"] == subject


def test_ses_sdk_send(sqrzl_server):
    sqrzl_server.require_provider("ses")
    boto3 = pytest.importorskip("boto3")
    botocore_config = pytest.importorskip("botocore.config")

    mailbox, subject = _mailbox_subject("ses")
    client = boto3.client(
        "sesv2",
        endpoint_url=sqrzl_server.api_url,
        aws_access_key_id=sqrzl_server.access_key_id,
        aws_secret_access_key=sqrzl_server.secret_access_key,
        region_name="us-east-1",
        config=botocore_config.Config(signature_version="s3v4"),
    )

    response = client.send_email(
        FromEmailAddress="sender@example.com",
        Destination={"ToAddresses": [mailbox]},
        Content={
            "Simple": {
                "Subject": {"Data": subject},
                "Body": {"Text": {"Data": f"SDK SES body for {subject}"}},
            }
        },
    )

    assert "MessageId" in response

    messages = _wait_for_messages(sqrzl_server, mailbox)
    assert messages, f"no messages found for mailbox {mailbox}"
    detail = _get_mailbox_message(sqrzl_server, mailbox, messages[0]["message_id"])
    assert detail["subject"] == subject


def test_azure_communication_email_sdk_send(sqrzl_server):
    sqrzl_server.require_provider("acs")
    email_mod = pytest.importorskip("azure.communication.email")
    azure_transport = pytest.importorskip("azure.core.pipeline.transport")

    mailbox, subject = _mailbox_subject("acs")
    access_key = base64.b64encode(b"shared-secret").decode("ascii")
    os.environ["SQRZL_ACS_CONNECTION_STRING"] = (
        f"endpoint={sqrzl_server.api_url};accesskey={access_key}"
    )

    class LocalHttpTransport(azure_transport.RequestsTransport):
        def send(self, request, **kwargs):
            # The ACS connection-string constructor normalizes endpoints to
            # HTTPS. The emulator intentionally serves plain HTTP, so rewrite
            # only at the transport boundary after the official HMAC policy has
            # signed the request.
            if request.url.startswith("https://"):
                request.url = "http://" + request.url.removeprefix("https://")
            return super().send(request, **kwargs)

    client = email_mod.EmailClient.from_connection_string(
        os.environ["SQRZL_ACS_CONNECTION_STRING"],
        transport=LocalHttpTransport(),
    )

    message = {
        "senderAddress": "sender@example.com",
        "recipients": {"to": [{"address": mailbox}]},
        "content": {
            "subject": subject,
            "plainText": f"SDK ACS body for {subject}",
            "html": f"<p>SDK ACS body for {subject}</p>",
        },
    }

    poller = client.begin_send(message)
    poller.result(timeout=10)

    messages = _wait_for_messages(sqrzl_server, mailbox)
    assert messages, f"no messages found for mailbox {mailbox}"
    detail = _get_mailbox_message(sqrzl_server, mailbox, messages[0]["message_id"])
    assert detail["subject"] == subject
