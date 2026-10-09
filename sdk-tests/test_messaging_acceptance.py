from __future__ import annotations

import base64
import json
import smtplib
import uuid
from datetime import datetime, timezone

import pytest

from conftest import ACS_ACCESS_KEY, TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN
from test_email_sdk import _mailbox_subject, _list_mailbox_messages
from test_sms_sdk import _messages_for_peer
from test_storage_acceptance import _restart


def _aws(server, service, wrong=False):
    import boto3
    from botocore.config import Config

    return boto3.client(
        service,
        endpoint_url=server.api_url,
        aws_access_key_id=server.access_key_id,
        aws_secret_access_key="wrong-key" if wrong else server.secret_access_key,
        region_name="us-east-1",
        config=Config(signature_version="s3v4", retries={"max_attempts": 0}),
    )


def _acs(server, kind, wrong=False):
    from azure.core.pipeline.transport import RequestsTransport
    from azure.communication.email import EmailClient
    from azure.communication.sms import SmsClient

    class LocalHttpTransport(RequestsTransport):
        def send(self, request, **kwargs):
            request.url = request.url.replace("https://", "http://", 1)
            return super().send(request, **kwargs)

    key = base64.b64encode(b"wrong-key").decode() if wrong else ACS_ACCESS_KEY
    client_type = EmailClient if kind == "email" else SmsClient
    return client_type.from_connection_string(
        f"endpoint={server.api_url};accesskey={key}", transport=LocalHttpTransport()
    )


def test_sendgrid_native_auth_and_attachment_error(sqrzl_server):
    sqrzl_server.require_provider("sendgrid")
    sqrzl_server.require_messaging_auth()
    from sendgrid import SendGridAPIClient
    from python_http_client.exceptions import HTTPError

    mailbox, subject = _mailbox_subject("sendgrid-auth")
    body = {
        "from": {"email": "sender@example.com"},
        "personalizations": [{"to": [{"email": mailbox}]}],
        "subject": subject,
        "content": [{"type": "text/plain", "value": "native SDK"}],
    }
    client = SendGridAPIClient(sqrzl_server.sendgrid_api_key)
    client.client.host = sqrzl_server.api_url
    assert client.send(body).status_code == 202
    before = [
        item["message_id"] for item in _list_mailbox_messages(sqrzl_server, mailbox)
    ]
    wrong = SendGridAPIClient("SG.wrong-key")
    wrong.client.host = sqrzl_server.api_url
    with pytest.raises(HTTPError) as denied:
        wrong.send(body)
    assert denied.value.status_code == 401
    assert json.loads(denied.value.body)["errors"][0]["field"] == "authorization"
    invalid = {
        **body,
        "attachments": [
            {
                "filename": "file;injection.txt",
                "type": "text/plain",
                "content": "aGVsbG8=",
            }
        ],
    }
    with pytest.raises(HTTPError) as invalid_attachment:
        client.send(invalid)
    assert invalid_attachment.value.status_code == 400
    assert (
        "filename" in json.loads(invalid_attachment.value.body)["errors"][0]["message"]
    )
    assert [
        item["message_id"] for item in _list_mailbox_messages(sqrzl_server, mailbox)
    ] == before


def test_twilio_native_auth_and_body_boundary(sqrzl_server):
    sqrzl_server.require_provider("twilio")
    sqrzl_server.require_messaging_auth()
    from twilio.rest import Client
    from twilio.base.exceptions import TwilioRestException

    peer = "+1555" + str(uuid.uuid4().int % 10_000_000).zfill(7)
    client = Client(TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN)
    client.api.base_url = sqrzl_server.api_url
    accepted = client.messages.create(to=peer, from_="+15550001000", body="x" * 1600)
    assert accepted.sid.startswith("SM")
    before = [item["message_id"] for item in _messages_for_peer(sqrzl_server, peer)]
    wrong = Client(TWILIO_ACCOUNT_SID, "wrong-key")
    wrong.api.base_url = sqrzl_server.api_url
    with pytest.raises(TwilioRestException) as denied:
        wrong.messages.create(to=peer, from_="+15550001000", body="unauthorized")
    assert denied.value.status == 401
    assert denied.value.code == 20003
    with pytest.raises(TwilioRestException) as invalid:
        client.messages.create(to=peer, from_="+15550001000", body="x" * 1601)
    assert invalid.value.status == 400
    assert invalid.value.code == 21617
    assert [
        item["message_id"] for item in _messages_for_peer(sqrzl_server, peer)
    ] == before


def test_ses_native_auth_and_process_restart(sqrzl_server, record_property):
    sqrzl_server.require_provider("ses")
    sqrzl_server.require_messaging_auth()
    sqrzl_server.require_process()
    from botocore.exceptions import ClientError

    mailbox, subject = _mailbox_subject("ses-auth")
    body = {
        "FromEmailAddress": "sender@example.com",
        "Destination": {"ToAddresses": [mailbox]},
        "Content": {
            "Simple": {
                "Subject": {"Data": subject},
                "Body": {"Text": {"Data": "native SDK"}},
            }
        },
    }
    accepted = _aws(sqrzl_server, "sesv2").send_email(**body)
    before = [
        item["message_id"] for item in _list_mailbox_messages(sqrzl_server, mailbox)
    ]
    assert before == [accepted["MessageId"]]
    with pytest.raises(ClientError) as denied:
        _aws(sqrzl_server, "sesv2", wrong=True).send_email(**body)
    assert denied.value.response["ResponseMetadata"]["HTTPStatusCode"] == 403
    assert (
        denied.value.response["Error"]["Code"] == "MissingAuthenticationTokenException"
    )
    _restart(sqrzl_server, record_property)
    assert [
        item["message_id"] for item in _list_mailbox_messages(sqrzl_server, mailbox)
    ] == before


def test_sns_native_auth_and_subject_boundary(sqrzl_server):
    sqrzl_server.require_provider("sns")
    sqrzl_server.require_messaging_auth()
    from botocore.exceptions import ClientError

    peer = "+1555" + str(uuid.uuid4().int % 10_000_000).zfill(7)
    client = _aws(sqrzl_server, "sns")
    response = client.publish(PhoneNumber=peer, Message="native SDK", Subject="x" * 99)
    assert response["MessageId"]
    before = [item["message_id"] for item in _messages_for_peer(sqrzl_server, peer)]
    with pytest.raises(ClientError) as denied:
        _aws(sqrzl_server, "sns", wrong=True).publish(
            PhoneNumber=peer, Message="unauthorized"
        )
    assert denied.value.response["ResponseMetadata"]["HTTPStatusCode"] == 403
    assert denied.value.response["Error"]["Code"] == "AuthorizationError"
    with pytest.raises(ClientError) as invalid:
        client.publish(PhoneNumber=peer, Message="invalid", Subject="x" * 100)
    assert invalid.value.response["ResponseMetadata"]["HTTPStatusCode"] == 400
    assert invalid.value.response["Error"]["Code"] == "InvalidParameter"
    assert [
        item["message_id"] for item in _messages_for_peer(sqrzl_server, peer)
    ] == before


def test_sms_voice_native_auth_and_destination_error(sqrzl_server):
    sqrzl_server.require_provider("aws-sms-voice-v2")
    sqrzl_server.require_messaging_auth()
    from botocore.exceptions import ClientError

    peer = "+1555" + str(uuid.uuid4().int % 10_000_000).zfill(7)
    body = {
        "DestinationPhoneNumber": peer,
        "MessageBody": "native SDK",
        "OriginationIdentity": "+15550001000",
    }
    client = _aws(sqrzl_server, "pinpoint-sms-voice-v2")
    assert client.send_text_message(**body)["MessageId"]
    before = [item["message_id"] for item in _messages_for_peer(sqrzl_server, peer)]
    with pytest.raises(ClientError) as denied:
        _aws(sqrzl_server, "pinpoint-sms-voice-v2", wrong=True).send_text_message(
            **body
        )
    assert denied.value.response["ResponseMetadata"]["HTTPStatusCode"] == 400
    assert denied.value.response["Error"]["Code"] == "AccessDeniedException"
    with pytest.raises(ClientError) as invalid:
        client.send_text_message(**{**body, "DestinationPhoneNumber": "not-a-number"})
    assert invalid.value.response["ResponseMetadata"]["HTTPStatusCode"] == 400
    assert invalid.value.response["Error"]["Code"] == "ValidationException"
    assert [
        item["message_id"] for item in _messages_for_peer(sqrzl_server, peer)
    ] == before


def test_acs_email_native_auth_repeatability_and_process_restart(
    sqrzl_server, record_property
):
    sqrzl_server.require_provider("acs")
    sqrzl_server.require_messaging_auth()
    sqrzl_server.require_process()
    from azure.core.exceptions import HttpResponseError

    mailbox, subject = _mailbox_subject("acs-email-auth")
    body = {
        "senderAddress": "sender@example.com",
        "recipients": {"to": [{"address": mailbox}]},
        "content": {"subject": subject, "plainText": "native SDK"},
    }
    headers = {
        "Repeatability-Request-ID": str(uuid.uuid4()),
        "Repeatability-First-Sent": datetime.now(timezone.utc).strftime(
            "%a, %d %b %Y %H:%M:%S GMT"
        ),
    }
    operation = str(uuid.uuid4())
    client = _acs(sqrzl_server, "email")
    accepted = client.begin_send(
        body, operation_id=operation, headers=headers, polling=False
    ).result()
    before = [
        item["message_id"] for item in _list_mailbox_messages(sqrzl_server, mailbox)
    ]
    assert len(before) == 1
    with pytest.raises(HttpResponseError) as denied:
        _acs(sqrzl_server, "email", wrong=True).begin_send(body, polling=False)
    assert denied.value.status_code == 401
    assert denied.value.error.code == "Unauthorized"
    _restart(sqrzl_server, record_property)
    replay = client.begin_send(
        body, operation_id=operation, headers=headers, polling=False
    ).result()
    assert replay["id"] == accepted["id"]
    with pytest.raises(HttpResponseError) as conflict:
        client.begin_send(
            {**body, "content": {"subject": "changed", "plainText": "native SDK"}},
            operation_id=operation,
            headers=headers,
            polling=False,
        )
    assert conflict.value.status_code == 400
    assert conflict.value.error.code == "InvalidRequest"
    assert [
        item["message_id"] for item in _list_mailbox_messages(sqrzl_server, mailbox)
    ] == before


def test_acs_sms_native_auth_and_process_restart(sqrzl_server, record_property):
    sqrzl_server.require_provider("acs")
    sqrzl_server.require_messaging_auth()
    sqrzl_server.require_process()
    from azure.core.exceptions import HttpResponseError

    peer = "+1555" + str(uuid.uuid4().int % 10_000_000).zfill(7)
    accepted = _acs(sqrzl_server, "sms").send(
        from_="+15550001000", to=[peer], message="native SDK"
    )
    assert accepted[0].successful
    before = [item["message_id"] for item in _messages_for_peer(sqrzl_server, peer)]
    with pytest.raises(HttpResponseError) as denied:
        _acs(sqrzl_server, "sms", wrong=True).send(
            from_="+15550001000", to=[peer], message="unauthorized"
        )
    assert denied.value.status_code == 401
    assert denied.value.error.code == "Unauthorized"
    _restart(sqrzl_server, record_property)
    after = _messages_for_peer(sqrzl_server, peer)
    assert [item["message_id"] for item in after] == before
    assert after[0]["provider_message_id"] == accepted[0].message_id


def test_smtp_data_argument_and_line_boundary(sqrzl_server):
    sqrzl_server.require_provider("smtp")
    mailbox, subject = _mailbox_subject("smtp-contract")
    with smtplib.SMTP("127.0.0.1", sqrzl_server.smtp_port, timeout=5) as client:
        assert client.ehlo("sdk.local")[0] == 250
        assert client.mail("sender@example.com")[0] == 250
        assert client.rcpt(mailbox)[0] == 250
        assert client.docmd("DATA", "unexpected")[0] == 501
        assert client.docmd("NOOP", "x" * 505)[0] == 250
        assert client.docmd("NOOP", "x" * 506)[0] == 500
        assert _list_mailbox_messages(sqrzl_server, mailbox) == []
        assert (
            client.data(
                f"From: sender@example.com\r\nTo: {mailbox}\r\nSubject: {subject}\r\n\r\nnative SMTP"
            )[0]
            == 250
        )
    assert len(_list_mailbox_messages(sqrzl_server, mailbox)) == 1
