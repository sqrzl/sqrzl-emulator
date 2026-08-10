# SMS and MMS emulation

Sqrzl captures outbound text requests on the existing API listener and exposes
bidirectional inspection and simulation on the existing admin listener. It does
not open another port.

Supported provider entry points are:

- Twilio `POST /2010-04-01/Accounts/{AccountSid}/Messages.json` for SMS and
  MMS URL references.
- Amazon SNS Query `Publish` with a direct `PhoneNumber`.
- AWS End User Messaging SMS Voice v2 JSON protocol `SendTextMessage` and
  `SendMediaMessage` targets.
- Azure Communication Services `POST /sms?api-version=...` including
  one-to-many recipients.

The accepted request fields are deliberately narrower than each provider's full
product surface and are validated before capture:

- Twilio accepts `To`, `From`, `MessagingServiceSid`, `Body`, repeated
  `MediaUrl`, and `StatusCallback`. Status callbacks must pass Sqrzl's local
  callback-host allowlist. Service-selected senders initially return
  `accepted`/zero segments; explicit senders return `queued`.
- SNS accepts direct `PhoneNumber` Publish with raw messages or
  `MessageStructure=json`, plus up to ten MessageAttributes. It applies the
  `sms` value with `default` fallback, ignores irrelevant structured protocol
  keys as SNS does, and rejects duplicate JSON keys.
- AWS SMS Voice v2 accepts the documented fields and constraints of
  `SendTextMessage` and `SendMediaMessage`; media values are S3 references and
  `DryRun` validates without capture.
- ACS SMS accepts API versions `2021-03-07` and `2026-01-23`, 1-100 recipients,
  per-recipient repeatability data, and delivery-report/tag/timeout options.
  `messagingConnect` is explicitly unsupported.

Malformed requests on these endpoints receive the provider's normal error
envelope and never fall through to object-storage routing. Unsupported fields
are rejected rather than silently dropped, except where ignoring a field is the
documented provider behavior.

The Texts admin surface can configure a callback destination per provider and
local number, inject an inbound message, explicitly transition outbound delivery
to delivered or failed, inspect every callback request and bounded response, and
create linked retries. Callback failures never roll back messages or terminal
delivery state. Callback redirects are not followed. Callback hosts default to
`localhost`, `127.0.0.1`, and `::1`; add comma-separated hosts with
`SQRZL_TEXT_CALLBACK_ALLOWED_HOSTS`. `SQRZL_TEXT_CALLBACK_TIMEOUT_MS` defaults
to 5000.

Set both `SQRZL_TWILIO_ACCOUNT_SID` and `SQRZL_TWILIO_AUTH_TOKEN` to require
Twilio Basic authentication and sign inbound/status callback forms. AWS adapters
reuse `SQRZL_ACCESS_KEY_ID` and `SQRZL_SECRET_ACCESS_KEY`. ACS SMS reuses
`SQRZL_ACS_CONNECTION_STRING`.

Twilio inbound simulations may include base64 media. Sqrzl persists those bytes
and serves authenticated Twilio-shaped media URLs. Outbound Twilio media URLs
and AWS S3 media URIs are references only and are never fetched. AWS inbound MMS
and all ACS MMS requests are rejected.

This increment intentionally does not support SNS topic fan-out or subscription
management, production SNS certificate signing, AWS inbound MMS, ACS MMS, TwiML
execution, WhatsApp, RCS, voice calls, automatic delivery timing, remote media
fetching, or full provider management APIs.
