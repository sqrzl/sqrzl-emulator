"""Reviewed operation assertions for the versioned local-core-v1 subset."""

from __future__ import annotations

import base64
from dataclasses import replace
import io
import smtplib
import uuid

import pytest

from test_s3_sdk import _client as s3_client
from test_azure_sdk import _service as azure_service
from test_oci_sdk import _client as oci_client
from test_email_sdk import _list_mailbox_messages

PROFILE = "local-core-v1"
SMTP_SELECTORS = {"protocol": "SMTP/RFC5321-selected-plaintext"}


def receipt(record_property, operation, checks, selectors, *, native_auth=False):
    record_property(
        "operation_certification",
        {
            "profile": PROFILE,
            "operations": {
                operation: {
                    "checks": checks,
                    "selectors": selectors,
                    "native_auth": native_auth,
                }
            },
        },
    )


def smtp_client(server):
    server.require_provider("smtp")
    return smtplib.SMTP("127.0.0.1", server.smtp_port, timeout=10)


def envelope(client, mailbox):
    assert client.ehlo("cert.local")[0] == 250
    assert client.mail("sender@example.test")[0] == 250
    assert client.rcpt(mailbox)[0] == 250


def test_smtp_session_certification(sqrzl_server, record_property):
    with smtp_client(sqrzl_server) as client:
        assert client.mail("sender@example.test")[0] == 503
        assert client.helo("cert.local")[0] == 250
        assert client.ehlo("cert.local")[0] == 250
        assert "auth" not in client.esmtp_features
        assert "starttls" not in client.esmtp_features
        assert client.docmd("EHLO")[0] == 501
        assert client.docmd("HELO", "two domains")[0] == 501
        assert client.ehlo("x" * 505)[0] == 250
        assert client.ehlo("x" * 506)[0] == 500
        assert client.mail("sender@example.test")[0] == 250
        assert client.rcpt("reset@example.test")[0] == 250
        assert client.ehlo("cert.local")[0] == 250
        assert client.docmd("DATA")[0] == 503
    receipt(
        record_property,
        "smtp.Session.smtp.HELO-EHLO",
        ["positive", "negative", "condition", "boundary", "native-error", "resources"],
        SMTP_SELECTORS,
    )


def test_smtp_envelope_certification(sqrzl_server, record_property):
    mailbox = f"cert-envelope-{uuid.uuid4().hex}@example.test"
    with smtp_client(sqrzl_server) as client:
        assert client.rcpt(mailbox)[0] == 503
        assert client.ehlo("cert.local")[0] == 250
        assert client.mail("sender@example.test", options=["SIZE=12"])[0] == 555
        assert client.mail("")[0] == 250
        assert client.docmd("RCPT", "TO:<unterminated")[0] == 501
        for _ in range(1000):
            assert client.rcpt(mailbox)[0] == 250
        assert client.rcpt("excess@example.test")[0] == 452
        assert client.rset()[0] == 250
        assert client.mail("sender@example.test")[0] == 250
        assert client.rcpt(mailbox)[0] == 250
        assert client.data(f"Subject: bounded envelope\r\n\r\naccepted")[0] == 250
    assert len(_list_mailbox_messages(sqrzl_server, mailbox)) == 1
    receipt(
        record_property,
        "smtp.Envelope.smtp.default",
        ["positive", "negative", "condition", "boundary", "native-error", "resources"],
        SMTP_SELECTORS,
    )


def test_smtp_noop_certification(sqrzl_server, record_property):
    mailbox = f"cert-noop-{uuid.uuid4().hex}@example.test"
    with smtp_client(sqrzl_server) as client:
        envelope(client, mailbox)
        assert client.noop()[0] == 250
        assert client.docmd("NOOP", "x" * 505)[0] == 250
        assert client.docmd("NOOP", "x" * 506)[0] == 500
        assert client.noop()[0] == 250
        assert client.data("Subject: noop preserved envelope\r\n\r\naccepted")[0] == 250
    assert len(_list_mailbox_messages(sqrzl_server, mailbox)) == 1
    receipt(
        record_property,
        "smtp.NOOP.smtp.command",
        ["positive", "negative", "condition", "boundary", "native-error", "resources"],
        SMTP_SELECTORS,
    )


def test_smtp_rset_certification(sqrzl_server, record_property):
    mailbox = f"cert-rset-{uuid.uuid4().hex}@example.test"
    with smtp_client(sqrzl_server) as client:
        envelope(client, mailbox)
        assert client.docmd("RSET", "unexpected")[0] == 501
        assert client.docmd("RSET", "x" * 506)[0] == 500
        assert (
            client.data("Subject: invalid reset preserved envelope\r\n\r\naccepted")[0]
            == 250
        )
        assert client.mail("sender@example.test")[0] == 250
        assert client.rcpt(mailbox)[0] == 250
        assert client.rset()[0] == 250
        assert client.docmd("DATA")[0] == 503
    assert len(_list_mailbox_messages(sqrzl_server, mailbox)) == 1
    receipt(
        record_property,
        "smtp.RSET.smtp.command",
        ["positive", "negative", "condition", "boundary", "native-error", "resources"],
        SMTP_SELECTORS,
    )


def test_smtp_quit_certification(sqrzl_server, record_property):
    with smtp_client(sqrzl_server) as client:
        assert client.docmd("QUIT", "unexpected")[0] == 501
        assert client.docmd("QUIT", "x" * 506)[0] == 500
        assert client.noop()[0] == 250
        assert client.quit()[0] == 221
        with pytest.raises(smtplib.SMTPServerDisconnected):
            client.noop()
    receipt(
        record_property,
        "smtp.QUIT.smtp.command",
        ["positive", "negative", "condition", "boundary", "native-error", "resources"],
        SMTP_SELECTORS,
    )


def test_smtp_data_certification(sqrzl_server, record_property):
    mailbox = f"cert-data-{uuid.uuid4().hex}@example.test"
    with smtp_client(sqrzl_server) as client:
        assert client.docmd("DATA")[0] == 503
        envelope(client, mailbox)
        assert client.docmd("DATA", "unexpected")[0] == 501
        assert (
            client.data("Subject: certified data\r\n\r\n.first\r\n..second\r\nlast")[0]
            == 250
        )
    messages = _list_mailbox_messages(sqrzl_server, mailbox)
    assert len(messages) == 1
    message_id = messages[0]["message_id"]
    from test_email_sdk import _get_mailbox_message

    detail = _get_mailbox_message(sqrzl_server, mailbox, message_id)
    assert ".first" in detail["body_text"]
    assert "..second" in detail["body_text"]
    receipt(
        record_property,
        "smtp.Data.smtp.default",
        ["positive", "negative", "condition", "boundary", "native-error"],
        SMTP_SELECTORS,
    )


def test_s3_current_head_certification(sqrzl_server, record_property):
    sqrzl_server.require_provider("s3")
    sqrzl_server.require_process()
    from botocore.exceptions import ClientError

    client = s3_client(sqrzl_server)
    assert client.meta.service_model.api_version == "2006-03-01"
    bucket = sqrzl_server.bucket_name("cert-s3-head")
    client.create_bucket(Bucket=bucket)
    uploaded = client.put_object(
        Bucket=bucket,
        Key="current",
        Body=b"metadata",
        ContentType="text/plain",
        Metadata={"owner": "certified"},
    )

    def verify():
        head = client.head_object(
            Bucket=bucket, Key="current", IfMatch=uploaded["ETag"]
        )
        assert head["ContentLength"] == 8 and head["ContentType"] == "text/plain"
        assert head["ETag"] == uploaded["ETag"] and head["Metadata"] == {
            "owner": "certified"
        }
        assert "Body" not in head
        for arguments, status in [
            ({"IfMatch": '"wrong"'}, 412),
            ({"IfNoneMatch": uploaded["ETag"]}, 304),
        ]:
            with pytest.raises(ClientError) as error:
                client.head_object(Bucket=bucket, Key="current", **arguments)
            assert error.value.response["ResponseMetadata"]["HTTPStatusCode"] == status
            assert error.value.response["Error"]["Code"] == str(status)

    verify()
    with pytest.raises(ClientError) as missing:
        client.head_object(Bucket=bucket, Key="missing")
    assert missing.value.response["ResponseMetadata"]["HTTPStatusCode"] == 404
    assert missing.value.response["Error"]["Code"] == "404"
    if sqrzl_server.enforce_auth:
        with pytest.raises(ClientError) as denied:
            s3_client(replace(sqrzl_server, secret_access_key="incorrect")).head_object(
                Bucket=bucket, Key="current"
            )
        assert denied.value.response["ResponseMetadata"]["HTTPStatusCode"] == 403
        assert denied.value.response["Error"]["Code"] == "403"
    sqrzl_server.restart()
    verify()
    sqrzl_server.restart(kill=True)
    verify()
    client.delete_object(Bucket=bucket, Key="current")
    client.delete_bucket(Bucket=bucket)
    checks = [
        "positive",
        "negative",
        "condition",
        "boundary",
        "restart",
        "process-crash",
        "native-error",
    ]
    if sqrzl_server.enforce_auth:
        checks.append("native-auth")
    receipt(
        record_property,
        "s3-family.HeadObject.head.current",
        checks,
        {"api_version": "2006-03-01", "resource": "current"},
        native_auth=sqrzl_server.enforce_auth,
    )


def test_azure_current_head_certification(sqrzl_server, record_property):
    sqrzl_server.require_provider("azure")
    sqrzl_server.require_process()
    from azure.core import MatchConditions
    from azure.core.exceptions import HttpResponseError

    version = "2024-11-04"
    service = azure_service(sqrzl_server, api_version=version)
    container = service.create_container(sqrzl_server.bucket_name("cert-azure-head"))
    blob = container.get_blob_client("current")
    result = blob.upload_blob(b"metadata", metadata={"owner": "certified"})
    responses = []

    def capture(response):
        request = response.http_response.request
        assert request.method == "HEAD" and request.headers["x-ms-version"] == version
        assert response.http_response.headers["x-ms-version"] == version
        responses.append(response.http_response.status_code)

    def verify():
        props = blob.get_blob_properties(
            etag=result["etag"],
            match_condition=MatchConditions.IfNotModified,
            raw_response_hook=capture,
        )
        assert (
            props.size == 8
            and props.etag == result["etag"]
            and props.metadata == {"owner": "certified"}
        )
        with pytest.raises(HttpResponseError) as error:
            blob.get_blob_properties(
                etag='"wrong"',
                match_condition=MatchConditions.IfNotModified,
                raw_response_hook=capture,
            )
        assert (
            error.value.status_code == 412
            and error.value.error_code == "ConditionNotMet"
        )

    verify()
    with pytest.raises(HttpResponseError) as missing:
        container.get_blob_client("missing").get_blob_properties(
            raw_response_hook=capture
        )
    assert (
        missing.value.status_code == 404 and missing.value.error_code == "BlobNotFound"
    )
    if sqrzl_server.enforce_auth:
        wrong = azure_service(
            replace(
                sqrzl_server, azure_account_key=base64.b64encode(b"incorrect").decode()
            ),
            api_version=version,
        )
        with pytest.raises(HttpResponseError) as denied:
            wrong.get_blob_client(
                container.container_name, "current"
            ).get_blob_properties(raw_response_hook=capture)
        assert (
            denied.value.status_code == 403
            and denied.value.error_code == "AuthenticationFailed"
        )
    sqrzl_server.restart()
    verify()
    sqrzl_server.restart(kill=True)
    verify()
    assert 200 in responses and 404 in responses and 412 in responses
    blob.delete_blob()
    container.delete_container()
    checks = [
        "positive",
        "negative",
        "condition",
        "boundary",
        "restart",
        "process-crash",
        "native-error",
    ]
    if sqrzl_server.enforce_auth:
        checks.append("native-auth")
    receipt(
        record_property,
        "azure-blob.GetBlobProperties.head.current",
        checks,
        {"api_version": version, "resource": "current"},
        native_auth=sqrzl_server.enforce_auth,
    )


def test_oci_current_head_certification(sqrzl_server, tmp_path, record_property):
    sqrzl_server.require_provider("oci")
    sqrzl_server.require_process()
    import oci

    client = oci_client(sqrzl_server, tmp_path)
    namespace = client.get_namespace().data
    bucket = sqrzl_server.bucket_name("cert-oci-head")
    client.create_bucket(
        namespace,
        oci.object_storage.models.CreateBucketDetails(
            name=bucket, compartment_id="ocid1.compartment.oc1..sqrzl"
        ),
    )
    uploaded = client.put_object(
        namespace,
        bucket,
        "current",
        io.BytesIO(b"metadata"),
        opc_meta={"owner": "certified"},
    )
    etag = uploaded.headers["etag"]

    def verify():
        head = client.head_object(namespace, bucket, "current", if_match=etag)
        assert int(head.headers["content-length"]) == 8 and head.headers["etag"] == etag
        assert head.headers["opc-meta-owner"] == "certified" and head.data is None
        with pytest.raises(oci.exceptions.ServiceError) as error:
            client.head_object(namespace, bucket, "current", if_match='"wrong"')
        assert error.value.status == 412 and error.value.code is None

    verify()
    with pytest.raises(oci.exceptions.ServiceError) as missing:
        client.head_object(namespace, bucket, "missing")
    assert missing.value.status == 404 and missing.value.code is None
    if sqrzl_server.enforce_auth:
        wrong = oci_client(
            replace(sqrzl_server, oci_user_ocid="ocid1.user.oc1..incorrect"), tmp_path
        )
        with pytest.raises(oci.exceptions.ServiceError) as denied:
            wrong.head_object(namespace, bucket, "current")
        assert denied.value.status == 401 and denied.value.code is None
    sqrzl_server.restart()
    verify()
    sqrzl_server.restart(kill=True)
    verify()
    client.delete_object(namespace, bucket, "current")
    client.delete_bucket(namespace, bucket)
    checks = [
        "positive",
        "negative",
        "condition",
        "boundary",
        "restart",
        "process-crash",
        "native-error",
    ]
    if sqrzl_server.enforce_auth:
        checks.append("native-auth")
    receipt(
        record_property,
        "oci-object.HeadObject.head.default",
        checks,
        {"api_version": "20160918", "resource": "current"},
        native_auth=sqrzl_server.enforce_auth,
    )
