//! Minimal SMTP server for capturing outbound mail during local development.
//!
//! Implements just enough of RFC 5321 to accept a submission and hand it to the
//! [`MailStore`] fan-out path: `EHLO`/`HELO`, `MAIL FROM`, `RCPT TO`, `DATA`,
//! `RSET`, `NOOP`, `QUIT`. Plaintext only — no STARTTLS/TLS and no SMTP AUTH,
//! since this targets local dev/CI rather than production mail transport; real
//! MTAs are not advertised a STARTTLS capability, so they won't attempt to
//! upgrade the connection.

use crate::error::{Error, Result};
use crate::mail::model::{Address, Message, SourceProtocol};
use crate::mail::{fan_out, MailStore};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

pub struct SmtpServer {
    mail: Arc<dyn MailStore>,
    port: u16,
    max_message_bytes: usize,
}

impl SmtpServer {
    #[must_use]
    pub fn new(mail: Arc<dyn MailStore>, port: u16) -> Self {
        Self {
            mail,
            port,
            max_message_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        }
    }

    /// Applies the maximum accepted SMTP `DATA` payload size.
    #[must_use]
    pub fn with_max_message_bytes(mut self, max_message_bytes: usize) -> Self {
        self.max_message_bytes = max_message_bytes;
        self
    }

    /// Binds the configured port and serves connections until the listener errors.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub async fn start(self) -> Result<()> {
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], self.port));
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| Error::InternalError(e.to_string()))?;
        tracing::info!("SMTP server listening on 0.0.0.0:{}", self.port);

        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| Error::InternalError(e.to_string()))?;
            let mail = self.mail.clone();
            let max_message_bytes = self.max_message_bytes;
            tokio::spawn(async move {
                if let Err(err) = handle_session(stream, mail, max_message_bytes).await {
                    tracing::warn!("SMTP session error: {}", err);
                }
            });
        }
    }
}

#[derive(Default)]
struct Transaction {
    from: Option<Address>,
    recipients: Vec<Address>,
}

/// Drives one SMTP connection to completion. Generic over the stream type so
/// tests can exercise it over an in-memory `tokio::io::duplex` pipe instead of a
/// real socket.
#[allow(clippy::too_many_lines)] // One SMTP state machine keeps protocol and resource rejection/reset behavior together.
async fn handle_session<S>(
    stream: S,
    mail: Arc<dyn MailStore>,
    max_message_bytes: usize,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    write_line(&mut writer, "220 sqrzl-emulator SMTP ready").await?;

    let mut transaction = Transaction::default();
    let mut greeted = false;

    while let Some(line) = next_line(&mut reader).await? {
        let line = match line {
            CommandLine::Valid(line) => line,
            CommandLine::Invalid(message) => {
                write_line(&mut writer, message).await?;
                continue;
            }
        };
        let Some((command, rest)) = split_command(&line) else {
            write_line(&mut writer, "500 Command not recognized").await?;
            continue;
        };

        if let Some(error) = command_argument_error(&command, rest, line.contains(' ')) {
            write_line(&mut writer, error).await?;
            continue;
        }
        match command.as_str() {
            "EHLO" | "HELO" => {
                greeted = true;
                transaction = Transaction::default();
                write_line(&mut writer, "250 sqrzl-emulator").await?;
            }
            "MAIL" => {
                if !greeted || transaction.from.is_some() {
                    write_line(&mut writer, "503 Bad sequence of commands").await?;
                    continue;
                }
                match parse_address(rest, "FROM:", true) {
                    Some(address) => {
                        transaction.from = Some(address);
                        write_line(&mut writer, "250 OK").await?;
                    }
                    None if unsupported_envelope_parameters(rest) => {
                        write_line(&mut writer, "555 MAIL FROM parameters not supported").await?;
                    }
                    None => write_line(&mut writer, "501 Syntax error in MAIL FROM").await?,
                }
            }
            "RCPT" if transaction.from.is_none() => {
                write_line(&mut writer, "503 Bad sequence of commands").await?;
            }
            "RCPT" => match parse_address(rest, "TO:", false) {
                Some(address) => {
                    transaction.recipients.push(address);
                    write_line(&mut writer, "250 OK").await?;
                }
                None if unsupported_envelope_parameters(rest) => {
                    write_line(&mut writer, "555 RCPT TO parameters not supported").await?;
                }
                None => write_line(&mut writer, "501 Syntax error in RCPT TO").await?,
            },
            "DATA" => {
                if transaction.from.is_none() || transaction.recipients.is_empty() {
                    write_line(&mut writer, "503 Bad sequence of commands").await?;
                    continue;
                }
                write_line(&mut writer, "354 Start mail input; end with <CRLF>.<CRLF>").await?;
                let raw = match read_data(&mut reader, max_message_bytes).await {
                    Ok(Some(raw)) => raw,
                    Ok(None) => return Ok(()),
                    Err(Error::InvalidRequest(message)) => {
                        write_line(&mut writer, &format!("552 {message}")).await?;
                        transaction = Transaction::default();
                        continue;
                    }
                    Err(Error::CaptureTooLarge) => {
                        write_line(&mut writer, &format!("552 {}", Error::CaptureTooLarge)).await?;
                        transaction = Transaction::default();
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let message = build_message(&transaction, &raw);
                match fan_out(mail.as_ref(), &message) {
                    Ok(_) => write_line(&mut writer, "250 OK: message accepted").await?,
                    Err(Error::CaptureTooLarge) => {
                        write_line(&mut writer, &format!("552 {}", Error::CaptureTooLarge)).await?;
                    }
                    Err(err) => write_line(&mut writer, &format!("451 {err}")).await?,
                }
                transaction = Transaction::default();
            }
            "RSET" => {
                transaction = Transaction::default();
                write_line(&mut writer, "250 OK").await?;
            }
            "NOOP" => write_line(&mut writer, "250 OK").await?,
            "QUIT" => {
                write_line(&mut writer, "221 Bye").await?;
                break;
            }
            _ => write_line(&mut writer, "502 Command not implemented").await?,
        }
    }

    Ok(())
}

enum CommandLine {
    Valid(String),
    Invalid(&'static str),
}

/// Drain one command without retaining more than the RFC 5321 512-octet limit.
async fn next_line<R>(reader: &mut BufReader<R>) -> Result<Option<CommandLine>>
where
    R: AsyncRead + Unpin,
{
    let mut line = Vec::with_capacity(512);
    let mut too_long = false;
    loop {
        let byte = match reader.read_u8().await {
            Ok(byte) => byte,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(Error::InternalError(error.to_string())),
        };
        if line.len() < 512 {
            line.push(byte);
        } else {
            too_long = true;
        }
        if byte == b'\n' {
            break;
        }
    }
    if too_long {
        return Ok(Some(CommandLine::Invalid("500 Command line too long")));
    }
    if !line.ends_with(b"\r\n") {
        return Ok(Some(CommandLine::Invalid("501 Command must end with CRLF")));
    }
    line.truncate(line.len() - 2);
    Ok(Some(match String::from_utf8(line) {
        Ok(line) => CommandLine::Valid(line),
        Err(_) => CommandLine::Invalid("500 Command is not valid UTF-8"),
    }))
}

fn split_command(line: &str) -> Option<(String, &str)> {
    if line.is_empty() || line.starts_with(' ') || line.chars().any(char::is_control) {
        return None;
    }
    let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
    Some((command.to_ascii_uppercase(), rest.trim()))
}

fn command_argument_error(command: &str, rest: &str, has_parameters: bool) -> Option<&'static str> {
    if matches!(command, "EHLO" | "HELO")
        && (rest.is_empty() || rest.chars().any(char::is_whitespace))
    {
        Some("501 Domain/address required")
    } else if matches!(command, "DATA" | "RSET" | "QUIT") && has_parameters {
        Some("501 Command does not accept arguments")
    } else {
        None
    }
}

fn unsupported_envelope_parameters(rest: &str) -> bool {
    rest.split_once('>')
        .is_some_and(|(_, parameters)| parameters.starts_with(' ') && !parameters.trim().is_empty())
}

/// Parses `FROM:<addr>` / `TO:<addr>` envelope arguments, case-insensitively on
/// the prefix, tolerating an optional space before `<addr>`.
fn parse_address(rest: &str, prefix: &str, allow_null: bool) -> Option<Address> {
    let rest = rest.trim();
    // `get` (rather than slicing directly) avoids panicking on a UTF-8 boundary
    // if a malformed client sends multi-byte characters before the prefix.
    let head = rest.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let without_prefix = rest[prefix.len()..].trim();
    let path = without_prefix.strip_prefix('<')?;
    let close = path.find('>')?;
    let email = path[..close].trim();
    let parameters = &path[close + 1..];
    // No ESMTP extensions are advertised; accepting an extension parameter
    // would silently discard a requested delivery semantic.
    if !parameters.trim().is_empty() {
        return None;
    }
    if allow_null && email.is_empty() {
        // RFC 5321 reserves the null reverse-path for delivery-status and
        // other bounce traffic. An empty Address records that envelope value;
        // the outer Option still distinguishes it from MAIL not being issued.
        return Some(Address::new(""));
    }
    if email.is_empty() || email.contains(['<', '>']) || email.chars().any(char::is_whitespace) {
        return None;
    }
    Some(Address::new(email))
}

async fn read_data<R>(
    reader: &mut BufReader<R>,
    max_message_bytes: usize,
) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    // Even one-recipient capture reserves six message representations. Raw
    // MIME occupies at least two JSON bytes per input byte in each, plus two
    // raw sidecars: a lower bound of fourteen bytes per raw input byte. Reject
    // a necessarily oversized DATA payload before constructing body/MIME copies.
    let raw_capture_limit = usize::try_from(crate::capture::budget::MAX_CAPTURE_BYTES / 14)
        .expect("the fixed capture limit fits usize");
    let input_limit = max_message_bytes.min(raw_capture_limit);
    let mut raw = Vec::new();
    let mut line = Vec::new();
    let mut too_large = false;
    loop {
        let byte = match reader.read_u8().await {
            Ok(byte) => byte,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(Error::InternalError(error.to_string())),
        };

        // Once the limit is exceeded, retaining three bytes is sufficient to
        // recognize the SMTP terminator without buffering the rejected body.
        let line_limit = if too_large {
            3
        } else {
            input_limit.saturating_add(3)
        };
        if line.len() < line_limit {
            line.push(byte);
        } else {
            too_large = true;
            raw.clear();
        }

        if byte == b'\n' {
            let ended_with_crlf = line.ends_with(b"\r\n");
            if ended_with_crlf {
                line.truncate(line.len() - 2);
            } else {
                line.pop();
            }
            if ended_with_crlf && line == b"." {
                break;
            }
            if !too_large {
                let unescaped = line.strip_prefix(b".").unwrap_or(&line);
                let added = unescaped.len().saturating_add(1);
                if raw.len().saturating_add(added) > input_limit {
                    too_large = true;
                    raw.clear();
                } else {
                    raw.extend_from_slice(unescaped);
                    raw.push(b'\n');
                }
            }
            line.clear();
        }
    }
    if too_large && input_limit < max_message_bytes {
        Err(Error::CaptureTooLarge)
    } else if too_large {
        Err(Error::InvalidRequest(format!(
            "message exceeds the {max_message_bytes}-byte emulator limit"
        )))
    } else {
        Ok(Some(raw))
    }
}

async fn write_line<W>(writer: &mut W, line: &str) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(|e| Error::InternalError(e.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|e| Error::InternalError(e.to_string()))
}

/// Splits the raw DATA payload into headers/body and builds a [`Message`],
/// preferring the SMTP envelope From/To (what was actually transacted) over
/// header values, while still capturing headers verbatim for inspection.
fn build_message(transaction: &Transaction, raw: &[u8]) -> Message {
    let text = String::from_utf8_lossy(raw);
    let (header_block, body) = text.split_once("\n\n").unwrap_or((text.as_ref(), ""));

    let mut headers = HashMap::new();
    for line in header_block.lines() {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let from = parse_header_address(
        headers
            .get("from")
            .or_else(|| headers.get("x-envelope-from"))
            .map_or("unknown@localhost", |value| value),
    )
    .unwrap_or_else(|| Address::new("unknown@localhost"));
    let recipients = parse_header_address_list(
        headers
            .get("to")
            .or_else(|| headers.get("x-envelope-to"))
            .map_or("", |value| value),
    );
    let cc = parse_header_address_list(headers.get("cc").map_or("", |value| value));
    let bcc = parse_header_address_list(headers.get("bcc").map_or("", |value| value));
    let subject = headers.get("subject").cloned().unwrap_or_default();
    let from = transaction.from.clone().unwrap_or(from);

    Message {
        source_protocol: SourceProtocol::Smtp,
        from,
        to: if transaction.recipients.is_empty() {
            recipients
        } else {
            transaction.recipients.clone()
        },
        cc,
        bcc,
        reply_to: Vec::new(),
        subject,
        headers,
        body_text: Some(body.trim().to_string()),
        body_html: None,
        attachments: Vec::new(),
        user_engagement_tracking_disabled: None,
        provider_metadata: HashMap::new(),
        raw_mime: Some(raw.to_vec()),
        thread_id: None,
    }
}

fn parse_header_address(value: &str) -> Option<Address> {
    let mut value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(start) = value.find('<') {
        if let Some(end) = value.rfind('>') {
            value = &value[start + 1..end];
        }
    }
    if value.is_empty() {
        return None;
    }
    Some(Address {
        email: value.trim().to_string(),
        name: None,
    })
}

fn parse_header_address_list(value: &str) -> Vec<Address> {
    let mut out = Vec::new();
    for raw in value.split(',') {
        if let Some(address) = parse_header_address(raw) {
            out.push(address);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::filesystem::FilesystemMailStore;
    use crate::mail::model::ListMessagesParams;
    use tokio::io::{AsyncBufReadExt, BufReader as ClientBufReader};

    fn temp_store() -> Arc<dyn MailStore> {
        let dir = std::env::temp_dir().join(format!("sqrzl-smtp-test-{}", uuid::Uuid::new_v4()));
        Arc::new(FilesystemMailStore::open(dir).expect("store should open"))
    }

    #[tokio::test]
    async fn should_capture_message_when_running_a_full_smtp_transaction() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);

        let session = tokio::spawn(handle_session(server, mail.clone(), 4096));
        let mut client_reader = ClientBufReader::new(client);

        // Greeting
        let mut greeting = String::new();
        client_reader.read_line(&mut greeting).await.unwrap();
        assert!(greeting.starts_with("220"));

        send(&mut client_reader, "EHLO client.example.com").await;
        assert_reply(&mut client_reader, "250").await;

        send(&mut client_reader, "MAIL FROM:<sender@example.com>").await;
        assert_reply(&mut client_reader, "250").await;

        send(&mut client_reader, "RCPT TO:<alice@example.com>").await;
        assert_reply(&mut client_reader, "250").await;

        send(&mut client_reader, "DATA").await;
        assert_reply(&mut client_reader, "354").await;

        send(&mut client_reader, "Subject: hello from smtp").await;
        send(&mut client_reader, "").await;
        send(&mut client_reader, "This is the body.").await;
        send(&mut client_reader, ".").await;
        assert_reply(&mut client_reader, "250").await;

        send(&mut client_reader, "QUIT").await;
        assert_reply(&mut client_reader, "221").await;

        session.await.expect("session task should not panic").ok();

        let result = mail
            .list_messages("alice@example.com", ListMessagesParams::default())
            .expect("list should succeed");
        assert_eq!(result.messages.len(), 1);
        let stored = &result.messages[0];
        assert_eq!(stored.message.from.email, "sender@example.com");
        assert_eq!(stored.message.subject, "hello from smtp");
        assert_eq!(
            stored.message.body_text.as_deref(),
            Some("This is the body.")
        );
    }

    #[tokio::test]
    async fn should_accept_the_rfc_5321_null_reverse_path() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(handle_session(server, mail.clone(), 4096));
        let mut client_reader = ClientBufReader::new(client);

        let mut greeting = String::new();
        client_reader.read_line(&mut greeting).await.unwrap();
        send(&mut client_reader, "EHLO client.example.com").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "MAIL FROM:<>").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "RCPT TO:<alice@example.com>").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "DATA").await;
        assert_reply(&mut client_reader, "354").await;
        send(&mut client_reader, "From: original@example.com").await;
        send(&mut client_reader, "Subject: delivery failure").await;
        send(&mut client_reader, "").await;
        send(&mut client_reader, "bounce").await;
        send(&mut client_reader, ".").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "QUIT").await;
        assert_reply(&mut client_reader, "221").await;
        session.await.expect("session task should not panic").ok();

        let messages = mail
            .list_messages("alice@example.com", ListMessagesParams::default())
            .expect("list should succeed");
        assert_eq!(messages.messages.len(), 1);
        assert_eq!(messages.messages[0].message.from.email, "");
    }

    #[tokio::test]
    async fn should_reject_data_when_no_recipient_was_given() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);

        let session = tokio::spawn(handle_session(server, mail.clone(), 4096));
        let mut client_reader = ClientBufReader::new(client);

        let mut greeting = String::new();
        client_reader.read_line(&mut greeting).await.unwrap();

        send(&mut client_reader, "EHLO client.example.com").await;
        assert_reply(&mut client_reader, "250").await;

        send(&mut client_reader, "MAIL FROM:<sender@example.com>").await;
        assert_reply(&mut client_reader, "250").await;

        send(&mut client_reader, "DATA").await;
        assert_reply(&mut client_reader, "503").await;

        send(&mut client_reader, "QUIT").await;
        assert_reply(&mut client_reader, "221").await;

        session.await.expect("session task should not panic").ok();
    }

    #[tokio::test]
    async fn should_reject_oversized_data_and_keep_the_session_usable() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(handle_session(server, mail.clone(), 8));
        let mut client_reader = ClientBufReader::new(client);

        let mut greeting = String::new();
        client_reader.read_line(&mut greeting).await.unwrap();
        send(&mut client_reader, "EHLO client.example.com").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "MAIL FROM:<sender@example.com>").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "RCPT TO:<alice@example.com>").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "DATA").await;
        assert_reply(&mut client_reader, "354").await;
        send(&mut client_reader, "payload-too-large").await;
        send(&mut client_reader, ".").await;
        assert_reply(&mut client_reader, "552").await;
        send(&mut client_reader, "QUIT").await;
        assert_reply(&mut client_reader, "221").await;

        session.await.expect("session task should not panic").ok();
        assert!(mail
            .list_messages("alice@example.com", ListMessagesParams::default())
            .unwrap()
            .messages
            .is_empty());
    }

    #[tokio::test]
    async fn should_reject_projected_smtp_capture_and_accept_a_later_transaction() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(handle_session(server, mail.clone(), 2 * 1024 * 1024));
        let mut reader = ClientBufReader::new(client);
        assert_reply(&mut reader, "220").await;
        send(&mut reader, "EHLO client.example.com").await;
        assert_reply(&mut reader, "250").await;
        send(&mut reader, "MAIL FROM:<sender@example.com>").await;
        assert_reply(&mut reader, "250").await;
        for n in 0..20 {
            send(&mut reader, &format!("RCPT TO:<recipient{n}@example.com>")).await;
            assert_reply(&mut reader, "250").await;
        }
        send(&mut reader, "DATA").await;
        assert_reply(&mut reader, "354").await;
        send(&mut reader, "Subject: capture resource limit").await;
        send(&mut reader, "").await;
        let line = "x".repeat(512);
        for _ in 0..2048 {
            send(&mut reader, &line).await;
        }
        send(&mut reader, ".").await;
        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        assert!(response.starts_with("552"));
        assert!(response.contains("local 64 MiB aggregate limit"));
        assert!(mail.list_mailboxes().unwrap().is_empty());
        send(&mut reader, "MAIL FROM:<sender@example.com>").await;
        assert_reply(&mut reader, "250").await;
        send(&mut reader, "RCPT TO:<retry@example.com>").await;
        assert_reply(&mut reader, "250").await;
        send(&mut reader, "DATA").await;
        assert_reply(&mut reader, "354").await;
        send(&mut reader, "Subject: retry").await;
        send(&mut reader, "").await;
        send(&mut reader, "small").await;
        send(&mut reader, ".").await;
        assert_reply(&mut reader, "250").await;
        send(&mut reader, "QUIT").await;
        assert_reply(&mut reader, "221").await;
        session.await.unwrap().unwrap();
        assert_eq!(mail.list_mailboxes().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn should_bound_raw_smtp_data_before_constructing_capture_copies() {
        let limit = usize::try_from(crate::capture::budget::MAX_CAPTURE_BYTES / 14).unwrap();
        let mut payload = vec![b'x'; limit + 1];
        payload.extend_from_slice(b"\r\n.\r\n");
        let mut reader = BufReader::new(payload.as_slice());
        assert!(matches!(
            read_data(&mut reader, 128 * 1024 * 1024).await,
            Err(Error::CaptureTooLarge)
        ));
    }

    #[tokio::test]
    async fn should_reject_out_of_sequence_envelope_commands() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(handle_session(server, mail, 4096));
        let mut client_reader = ClientBufReader::new(client);

        let mut greeting = String::new();
        client_reader.read_line(&mut greeting).await.unwrap();

        send(&mut client_reader, "RCPT TO:<alice@example.com>").await;
        assert_reply(&mut client_reader, "503").await;

        send(&mut client_reader, "EHLO client.example.com").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "MAIL FROM:<sender@example.com>").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "MAIL FROM:<other@example.com>").await;
        assert_reply(&mut client_reader, "503").await;

        send(&mut client_reader, "QUIT").await;
        assert_reply(&mut client_reader, "221").await;
        session.await.expect("session task should not panic").ok();
    }

    #[tokio::test]
    async fn should_discard_data_when_the_client_disconnects_before_the_terminator() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(handle_session(server, mail.clone(), 4096));
        let mut client_reader = ClientBufReader::new(client);

        let mut greeting = String::new();
        client_reader.read_line(&mut greeting).await.unwrap();
        send(&mut client_reader, "EHLO client.example.com").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "MAIL FROM:<sender@example.com>").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "RCPT TO:<alice@example.com>").await;
        assert_reply(&mut client_reader, "250").await;
        send(&mut client_reader, "DATA").await;
        assert_reply(&mut client_reader, "354").await;
        send(&mut client_reader, "Subject: incomplete").await;
        send(&mut client_reader, "").await;
        client_reader
            .get_mut()
            .shutdown()
            .await
            .expect("client should half-close");

        session.await.expect("session task should not panic").ok();
        assert!(mail
            .list_messages("alice@example.com", ListMessagesParams::default())
            .expect("list should succeed")
            .messages
            .is_empty());
    }

    #[tokio::test]
    async fn should_reject_data_arguments_without_resetting_the_transaction() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(handle_session(server, mail.clone(), 4096));
        let mut client = ClientBufReader::new(client);
        assert_reply(&mut client, "220").await;
        for command in [
            "EHLO localhost",
            "MAIL FROM:<sender@example.com>",
            "RCPT TO:<alice@example.com>",
        ] {
            send(&mut client, command).await;
            assert_reply(&mut client, "250").await;
        }
        for command in ["DATA unexpected-argument", "DATA ", "DATA  "] {
            send(&mut client, command).await;
            assert_reply(&mut client, "501").await;
        }
        assert!(mail
            .list_messages("alice@example.com", ListMessagesParams::default())
            .unwrap()
            .messages
            .is_empty());
        send(&mut client, "DATA").await;
        assert_reply(&mut client, "354").await;
        send(&mut client, "Subject: valid retry").await;
        send(&mut client, "").await;
        send(&mut client, "hello").await;
        send(&mut client, ".").await;
        assert_reply(&mut client, "250").await;
        send(&mut client, "QUIT").await;
        assert_reply(&mut client, "221").await;
        session.await.unwrap().unwrap();
        assert_eq!(
            mail.list_messages("alice@example.com", ListMessagesParams::default())
                .unwrap()
                .messages
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn should_bound_and_validate_commands_independently_of_data() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(handle_session(server, mail.clone(), 4096));
        let mut client = ClientBufReader::new(client);
        assert_reply(&mut client, "220").await;
        // RFC 5321 includes CRLF in the 512-octet command limit.
        send(&mut client, &format!("NOOP {}", "x".repeat(505))).await;
        assert_reply(&mut client, "250").await;
        send(&mut client, &format!("NOOP {}", "x".repeat(506))).await;
        assert_reply(&mut client, "500").await;
        send(&mut client, &"x".repeat(1_048_576)).await;
        assert_reply(&mut client, "500").await;
        client.write_all(b"EHLO localhost\n").await.unwrap();
        assert_reply(&mut client, "501").await;
        for command in [" EHLO localhost", "EHLO\tlocalhost", "EHLO local\rhost"] {
            send(&mut client, command).await;
            assert_reply(&mut client, "500").await;
        }
        for command in ["EHLO", "EHLO two domains", "RSET argument", "QUIT argument"] {
            send(&mut client, command).await;
            assert_reply(&mut client, "501").await;
        }
        send(&mut client, "EHLO localhost").await;
        assert_reply(&mut client, "250").await;
        send(&mut client, "MAIL FROM:<sender@example.com> SIZE=12").await;
        assert_reply(&mut client, "555").await;
        for command in ["MAIL FROM:<sender@example.com>garbage"] {
            send(&mut client, command).await;
            assert_reply(&mut client, "501").await;
        }
        send(&mut client, "QUIT").await;
        assert_reply(&mut client, "221").await;
        session.await.unwrap().unwrap();
        assert!(mail.list_mailboxes().unwrap().is_empty());
    }

    async fn send(client: &mut ClientBufReader<tokio::io::DuplexStream>, line: &str) {
        client
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("write should succeed");
    }

    async fn assert_reply(client: &mut ClientBufReader<tokio::io::DuplexStream>, code: &str) {
        let mut reply = String::new();
        client
            .read_line(&mut reply)
            .await
            .expect("read should succeed");
        assert!(
            reply.starts_with(code),
            "expected reply starting with {code}, got {reply:?}"
        );
    }

    #[test]
    fn should_parse_address_variants() {
        // Arrange
        // Act
        // Assert
        let parsed =
            parse_address("from: <sender@example.com>", "FROM:", true).expect("should parse");
        assert_eq!(parsed.email, "sender@example.com");

        let parsed = parse_address("TO:<alice@example.com>", "TO:", false).expect("should parse");
        assert_eq!(parsed.email, "alice@example.com");

        assert!(parse_address("TO:", "TO:", false).is_none());
        assert_eq!(
            parse_address("FROM:<>", "FROM:", true)
                .expect("null reverse-path should parse")
                .email,
            ""
        );
        assert!(parse_address("FROM:<> BODY=8BITMIME", "FROM:", true).is_none());
        assert!(parse_address("FROM:sender@example.com", "FROM:", true).is_none());
        assert!(parse_address("FROM:<sender@example.com>garbage", "FROM:", true).is_none());
    }
}
