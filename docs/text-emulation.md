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
