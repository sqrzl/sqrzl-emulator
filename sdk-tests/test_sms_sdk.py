from __future__ import annotations

import base64
import os
import urllib.parse
import urllib.error
import urllib.request

import pytest

from conftest import ACS_ACCESS_KEY, TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN
from test_email_sdk import _admin_json_request


def _messages_for_peer(server, peer: str) -> list[dict[str, object]]:
    encoded = urllib.parse.quote(peer, safe="")
    response = _admin_json_request(
        server,
        f"/admin/v1/text-conversations/{encoded}/messages",
    )
    return response.get("items", [])


def test_twilio_messages_sdk(sqrzl_server):
    sqrzl_server.require_provider("twilio")
    twilio = pytest.importorskip("twilio.rest")
    client = twilio.Client(TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN)
    client.api.base_url = sqrzl_server.api_url

    peer = "+15550001001"
    message = client.messages.create(
        to=peer,
        from_="+15550001000",
        body="Twilio SDK SMS",
        media_url=["https://example.invalid/one.jpg", "https://example.invalid/two.jpg"],
    )

    assert message.sid.startswith("SM")
    assert message.status == "queued"
    stored = _messages_for_peer(sqrzl_server, peer)
    assert any(item["provider_message_id"] == message.sid for item in stored)
    captured = next(item for item in stored if item["provider_message_id"] == message.sid)
    assert captured["channel"] == "mms"
    assert len(captured["media"]) == 2

    wrong_auth = base64.b64encode(b"wrong:credentials").decode("ascii")
    request = urllib.request.Request(
        f"{sqrzl_server.api_url}/2010-04-01/Accounts/{TWILIO_ACCOUNT_SID}/Messages.json",
        method="POST",
        headers={
            "authorization": f"Basic {wrong_auth}",
            "content-type": "application/x-www-form-urlencoded",
        },
        data=urllib.parse.urlencode(
            {"To": peer, "From": "+15550001000", "Body": "unauthorized"}
        ).encode(),
    )
    with pytest.raises(urllib.error.HTTPError) as rejected:
        urllib.request.urlopen(request)
    assert rejected.value.code == 401


def test_boto3_sns_direct_publish(sqrzl_server):
    sqrzl_server.require_provider("sns")
    boto3 = pytest.importorskip("boto3")
    peer = "+15550001002"
    client = boto3.client(
        "sns",
        endpoint_url=sqrzl_server.api_url,
        aws_access_key_id=sqrzl_server.access_key_id,
        aws_secret_access_key=sqrzl_server.secret_access_key,
        region_name="us-east-1",
    )

    response = client.publish(PhoneNumber=peer, Message="SNS SDK SMS")
    assert response["MessageId"]
    stored = _messages_for_peer(sqrzl_server, peer)
    assert any(item["provider_message_id"] == response["MessageId"] for item in stored)

def test_boto3_sms_voice_v2(sqrzl_server):
    sqrzl_server.require_provider("aws-sms-voice-v2")
    boto3 = pytest.importorskip("boto3")
    peer = "+15550001003"
    client = boto3.client(
        "pinpoint-sms-voice-v2",
        endpoint_url=sqrzl_server.api_url,
        aws_access_key_id=sqrzl_server.access_key_id,
        aws_secret_access_key=sqrzl_server.secret_access_key,
        region_name="us-east-1",
    )

    response = client.send_text_message(
        DestinationPhoneNumber=peer,
        OriginationIdentity="+15550001000",
        MessageBody="AWS SMS Voice SDK SMS",
        MessageType="TRANSACTIONAL",
    )
    assert response["MessageId"]
    stored = _messages_for_peer(sqrzl_server, peer)
    assert any(item["provider_message_id"] == response["MessageId"] for item in stored)

    media_peer = "+15550001013"
    media_response = client.send_media_message(
        DestinationPhoneNumber=media_peer,
        OriginationIdentity="+15550001000",
        MessageBody="AWS SMS Voice SDK MMS",
        MediaUrls=["s3://example-bucket/photo.jpg"],
    )
    media_stored = _messages_for_peer(sqrzl_server, media_peer)
    captured = next(
        item
        for item in media_stored
        if item["provider_message_id"] == media_response["MessageId"]
    )
    assert captured["channel"] == "mms"
    assert captured["media"][0]["external_url"] == "s3://example-bucket/photo.jpg"


def test_azure_communication_sms_sdk(sqrzl_server):
    sqrzl_server.require_provider("acs")
    sms_mod = pytest.importorskip("azure.communication.sms")
    azure_transport = pytest.importorskip("azure.core.pipeline.transport")

    class LocalHttpTransport(azure_transport.RequestsTransport):
        def send(self, request, **kwargs):
            if request.url.startswith("https://"):
                request.url = "http://" + request.url.removeprefix("https://")
            return super().send(request, **kwargs)

    connection_string = (
        f"endpoint={sqrzl_server.api_url};accesskey={ACS_ACCESS_KEY}"
    )
    os.environ["SQRZL_ACS_CONNECTION_STRING"] = connection_string
    client = sms_mod.SmsClient.from_connection_string(
        connection_string,
        transport=LocalHttpTransport(),
    )
    peers = ["+15550001004", "+15550001005"]
    results = client.send(
        from_="+15550001000",
        to=peers,
        message="ACS SDK SMS",
        enable_delivery_report=True,
    )

    assert len(results) == 2
    assert all(result.successful for result in results)
    stored = [_messages_for_peer(sqrzl_server, peer)[0] for peer in peers]
    assert stored[0]["batch_id"] == stored[1]["batch_id"]
