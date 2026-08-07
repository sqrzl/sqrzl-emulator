# Email Emulator Domain — Full Plan (SMTP, SendGrid, AWS SES, Azure Communication Services; GCP deferred)

## Context

Sqrzl today emulates one domain — blob storage — across four cloud-provider skins (S3, Azure Blob, GCS, OCI) sharing one `Storage` trait, one `ProviderAdapter` pattern, and a certification methodology (`compatibility-matrix.json` + interop/e2e/SDK test tiers) that keeps claims honest. The goal is to add a second domain, email, following that same discipline: a local dev/CI target that looks like real SMTP + SendGrid + SES + Azure Communication Services (ACS) from the outside, so developers/tests can send mail against `localhost` and inspect what "would have been sent" instead of hitting real providers.

This is the highest-leverage next domain because it's a pure wire primitive — send a thing, inspect what happened, no orchestration semantics — the same shape the existing certification machinery is built for. GCP has no infra-level transactional-email service comparable to SES/ACS (Gmail API is a Workspace/user-consent OAuth2 product, a fundamentally different auth model) — **decision: defer GCP/Gmail, track as `deferred` in the matrix, do not build in v1.**

## Architecture

Mail is a **new, parallel domain** — it does not reuse `Storage` or `ProviderAdapter` (both are bucket/key blob-shaped). New sibling module tree:

```
src/mail/
  mod.rs            // MailStore trait, StoredMessage/ListMessagesParams/Result types
  model.rs          // Message, Address, Attachment, DeliveryStatus/DeliveryState, SourceProtocol
  filesystem.rs      // FilesystemMailStore: MailStore impl backed by disk, under {blobs_path}/_mail/
  smtp.rs            // SmtpServer: raw-TCP accept loop + minimal SMTP state machine
  providers/
    mod.rs          // MailAdapter trait + MailAdapterRegistry (mirrors src/providers/mod.rs)
    sendgrid.rs     // SendGridAdapter — POST /v3/mail/send, Bearer auth
    ses.rs          // SesEmailAdapter — POST /v2/email/outbound-emails, SigV4 (reuse src/auth/sigv4.rs)
    acs.rs          // AcsEmailAdapter — POST /emails:send, new HMAC connection-string auth
```

**Mailbox model:** per-recipient mailbox (mailbox key = normalized lowercase address), directly analogous to per-bucket — mirrors the admin UI's bucket→objects drill-down and matches "show me what was sent to X." A multi-recipient message fans out on write: one stored copy per mailbox (To/Cc/Bcc), sharing `message_id`, each with its own delivery status. Plus a synthetic `"_all"` mailbox as an outbox-wide view (the "list_buckets at account level" equivalent).

**`MailStore` trait** (`src/mail/mod.rs`), same `crate::error::Result` convention as `Storage`:
```rust
pub trait MailStore: Send + Sync {
    fn store_message(&self, mailbox: &str, message: Message) -> Result<StoredMessage>;
    fn get_message(&self, mailbox: &str, message_id: &str) -> Result<StoredMessage>;
    fn list_messages(&self, mailbox: &str, params: ListMessagesParams) -> Result<ListMessagesResult>;
    fn delete_message(&self, mailbox: &str, message_id: &str) -> Result<()>;
    fn update_delivery_status(&self, mailbox: &str, message_id: &str, status: DeliveryStatus) -> Result<()>;
    fn list_mailboxes(&self) -> Result<Vec<MailboxInfo>>;
    fn ensure_mailbox(&self, mailbox: &str) -> Result<()>;
}
```

**Message model** (`src/mail/model.rs`), serde-serializable like `src/models/object.rs`: `message_id`, `source_protocol` (Smtp/SendGrid/Ses/Acs), `from`, `to`/`cc`/`bcc`, `subject`, raw `headers` map, `body_text`/`body_html`, `attachments`, `raw_mime` (full captured payload where applicable), `received_at`, `thread_id`.

**Filesystem layout:** `{blobs_path}/_mail/{mailbox}/{message_id}.json` (metadata) + `.raw` (large MIME/attachment payload), following `FilesystemStorage`'s existing small-metadata-JSON + separate-payload-file convention in a new namespace.

## Transport

**SMTP is the one genuinely new risk** — no hyper reuse, hand-rolled line protocol. New `SmtpServer` (`src/mail/smtp.rs`): `TcpListener::bind(SQRZL_SMTP_PORT)` + accept loop, spawn-per-connection, minimal state machine over `BufReader<TcpStream>`:

```
Greeting -> EHLO/HELO -> Ready
Ready: MAIL FROM:<addr> -> RCPT TO:<addr> (repeatable) -> DATA (read to "." terminator, parse headers+body)
       -> store_message() once per RCPT recipient -> 250 OK
       RSET/NOOP/QUIT handled minimally
```

Decisions: **plaintext only in v1** (no STARTTLS/TLS — advertise no STARTTLS in EHLO; acceptable for a local dev emulator, documented as a limitation). **No SMTP AUTH in v1** (unauthenticated relay, matching typical local-dev use). MIME parsing: prefer a small crate (check `Cargo.toml` for precedent on pulling in parsing deps, e.g. how Azure XML parsing is handled) over hand-rolling attachment/multipart parsing.

`main.rs` currently races exactly 2 futures via `tokio::select!` (API server, UI server). Adding SMTP means a 3rd listener — **switch to a spawn+join pattern** (`Vec` of `JoinHandle`s, first error/exit wins) rather than manually growing `select!` arms, since this scales cleanly if more listeners are ever added. Construct `Arc<dyn MailStore>` once in `main.rs`, pass to `SmtpServer` and to the API `Server` (for the HTTP-shaped mail adapters below).

**Config additions** (`src/config.rs`, following the exact `ENV_SQRZL_*` const + `from_env_with` pattern): `SQRZL_SMTP_PORT` (default `2525` — unprivileged port), `SQRZL_SENDGRID_API_KEY` (optional, unset ⇒ auth disabled for that adapter), `SQRZL_ACS_CONNECTION_STRING` (optional, same fallback). SES needs no new var — reuses `access_key_id`/`secret_access_key` via existing SigV4.

## HTTP-shaped provider adapters (SendGrid, SES, ACS)

New `MailAdapter` trait + `MailAdapterRegistry` (`src/mail/providers/mod.rs`), structurally identical to `ProviderAdapter`/`AdapterRegistry` but keyed on `Arc<dyn MailStore>` instead of `Arc<dyn Storage>`:

```rust
pub trait MailAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, req: &Request) -> bool;
    fn handle<'a>(&'a self, mail: Arc<dyn MailStore>, auth_config: Arc<AuthConfig>, req: Request)
        -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>>;
}
```

**Wiring:** route through the **existing API server/port** (`api_port`) — mail adapters get first chance to match (by path prefix: `/v3/mail/send`, `/v2/email/outbound-emails`, `/emails:send`) *before* falling through to the blob `AdapterRegistry`, since `S3Adapter::matches` is a catch-all. `Server` (`src/server/mod.rs`) gains `mail: Arc<dyn MailStore>` and `mail_adapters: Arc<MailAdapterRegistry>` fields. This keeps the "one API port for all provider HTTP APIs" model rather than adding a dedicated mail HTTP port.

| Provider | Match | Auth | Endpoint | Success response |
|---|---|---|---|---|
| SendGrid | path `/v3/mail/send` | `Authorization: Bearer <SQRZL_SENDGRID_API_KEY>` | `POST /v3/mail/send`, JSON personalizations/from/content | `202`, `X-Message-Id` header, empty body |
| SES v2 | path `/v2/email/outbound-emails` | AWS SigV4 (reuse `src/auth/sigv4.rs`), existing global key pair | `POST /v2/email/outbound-emails`, JSON | `{"MessageId": "..."}` |
| ACS | path `/emails:send` + `api-version` query param | HMAC-SHA256 over canonicalized request using `SQRZL_ACS_CONNECTION_STRING` (`endpoint=...;accesskey=...`) — new `src/auth/acs_hmac.rs`, mirroring `sigv4.rs`'s structure | `POST /emails:send`, JSON | `{"id": "...", "status": "Queued"}` |

Each adapter parses its provider-specific JSON → builds a `mail::model::Message` → `store_message()` once per recipient → renders the provider's real success/error envelope shape, following the same per-provider response-fidelity convention already used in `src/providers/{azure,gcs,oci}.rs`.

## Admin API + UI

Mirrors the bucket/object pattern exactly. New `src/api/admin/mail_route.rs` (separate from `route.rs` — unrelated routing concerns) dispatched from `handle_request` by path prefix `/admin/v1/mailboxes`. New `src/api/admin/mail_dto.rs` + additions to `src/api/models.rs` (`MailboxInfo`, `MessageSummary`, `MessageDetail`). Reuse `src/api/admin/pagination.rs` (`encode_next`/`parse_page_params`) verbatim.

Endpoints:
- `GET /admin/v1/mailboxes` — paginated list (address, message_count, last_received_at)
- `GET /admin/v1/mailboxes/{mailbox}/messages` — paginated summaries (id, from, subject, received_at, delivery_state)
- `GET /admin/v1/mailboxes/{mailbox}/messages/{id}` — full detail (headers, body)
- `GET /admin/v1/mailboxes/{mailbox}/messages/{id}/content` — raw MIME download
- `GET /admin/v1/mailboxes/{mailbox}/messages/{id}/attachments/{filename}`
- `DELETE /admin/v1/mailboxes/{mailbox}` and `.../messages/{id}`

`start_ui_server` (`src/api/server.rs`) threads `Arc<dyn MailStore>` through the same way `storage` is today.

UI (`ui/src`): new `features/mailboxes/`, `features/messages/` query hooks (mirroring `features/buckets/`, `features/objects/`); new `components/mail/*` (don't reuse `components/storage/*` — those are storage-typed, not domain-neutral); new `pages/app/mail/_routes.tsx` registering `/admin/mailboxes`, `/admin/mailboxes/{mailboxId}`, `/admin/mail/{mailboxId}/{messageId}`, mirroring the existing bucket/blob route triple. Before touching `ui/src/adapters/api.g.ts`, locate its generator source-of-truth (it appears generated) and regenerate rather than hand-edit.

## Certification plan

New top-level keys in `compatibility-matrix.json`, one per provider (matching existing `s3-family`/`azure-blob` granularity): `smtp`, `sendgrid`, `ses`, `acs`, `gmail`. Every non-deferred entry starts `status: "partial"`, `support_tier: "partial"` — **no `certified` claims in v1**, consistent with the rest of the project (everything else is `partial` today too). `gmail` gets one entry: `status: "deferred"`, `support_tier: "deferred"`, with a `limitations` note explaining the OAuth2 auth-model mismatch.

Required new tests (and `src/compatibility_matrix.rs`'s `known_verifiers()`/`known_sdk_verifiers()` allowlists must be updated for each new name cited):
- `tests/interop_email.rs` — in-process black-box tests per provider (SendGrid send+fan-out, SES SigV4 send, ACS HMAC send, SMTP transaction) — these back `verified_by`.
- `tests/e2e_email.rs` — real-socket round trip via `LiveServer`, including a full SMTP EHLO/MAIL FROM/RCPT TO/DATA/QUIT transcript.
- `sdk-tests/test_email_sdk.py` — official clients against a live spawned server (`conftest.py`'s `sqrzl_server` fixture): `smtplib` (stdlib), `sendgrid` python client, `boto3` `sesv2` client, `azure-communication-email` client — these back `sdk_verified_by`.
- Unit tests colocated per module, following the `should_<outcome>_given_<condition>_when_<action>` naming convention already in use.

CI (`ci.yml`) picks up new test files automatically via the existing `cargo test --workspace --all-targets --all-features` + Python SDK-cert steps; check `Dockerfile`/`compose.yml` port-exposure conventions and add `SQRZL_SMTP_PORT` there.

## Build order

1. **Domain scaffolding + SMTP** (`src/mail/{mod,model,filesystem,smtp}.rs`, `main.rs` multi-listener restructure, `SQRZL_SMTP_PORT`) — highest risk, unblocks everything else; ship with basic admin read API so captured mail is inspectable.
2. **SendGrid adapter** — simplest HTTP-shaped provider (bearer auth, no signing); validates `MailAdapter`/`MailAdapterRegistry` and the "mail-first" routing change in `src/server/mod.rs`.
3. **SES v2 adapter** — reuses existing SigV4 verification, so mainly new request/response shape work.
4. **ACS adapter** — new HMAC connection-string auth (`src/auth/acs_hmac.rs`), sequenced last among the three since it's the only genuinely new auth primitive.
5. **Admin UI** (mailboxes/messages pages) — can start once step 1's read API exists; reasonable to interleave with steps 2–4 rather than strictly sequence after.
6. **GCP/Gmail** — deferred; matrix entry only (`status`/`support_tier: "deferred"`), no code.

Certification (matrix entries + `known_verifiers()` updates) lands incrementally with each step's tests, staying `partial` throughout.

**Explicitly deferred beyond this plan:** STARTTLS/TLS, SMTP AUTH, GCP/Gmail emulation, sender-indexed mailbox views, cross-provider unified-inbox search.

## Verification

- `cargo test --workspace --all-targets --all-features` — new `interop_email`/`e2e_email` tests plus `compatibility_matrix.rs`'s validation tests (which will fail loudly if a new `verified_by`/`sdk_verified_by` entry references an unregistered test name).
- `cargo clippy --all-targets --all-features -D warnings -D clippy::pedantic` — matches existing CI gate.
- `python -m pytest sdk-tests/test_email_sdk.py` against a locally spawned server (via `conftest.py`'s fixture) — real `smtplib`/`sendgrid`/`boto3`/`azure-communication-email` clients.
- Manual smoke: `docker compose up --build`, then send via `smtplib` to `localhost:2525` and via each REST provider to `localhost:9000`, confirm each message appears in `GET /admin/v1/mailboxes/{mailbox}/messages` and in the new UI mail pages.
