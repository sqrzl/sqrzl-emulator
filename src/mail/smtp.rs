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
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
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

    while let Some(line) = next_line(&mut reader).await? {
        let Some((command, rest)) = split_command(&line) else {
            write_line(&mut writer, "500 Command not recognized").await?;
            continue;
        };

        match command.as_str() {
            "EHLO" | "HELO" => write_line(&mut writer, "250 sqrzl-emulator").await?,
            "MAIL" => {
                transaction = Transaction::default();
                match parse_address(rest, "FROM:") {
                    Some(address) => {
                        transaction.from = Some(address);
                        write_line(&mut writer, "250 OK").await?;
                    }
                    None => write_line(&mut writer, "501 Syntax error in MAIL FROM").await?,
                }
            }
            "RCPT" => match parse_address(rest, "TO:") {
                Some(address) => {
                    transaction.recipients.push(address);
                    write_line(&mut writer, "250 OK").await?;
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
                    Ok(raw) => raw,
                    Err(Error::InvalidRequest(message)) => {
                        write_line(&mut writer, &format!("552 {message}")).await?;
                        transaction = Transaction::default();
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let message = build_message(&transaction, &raw);
                match fan_out(mail.as_ref(), &message) {
                    Ok(_) => write_line(&mut writer, "250 OK: message accepted").await?,
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

async fn next_line<R>(reader: &mut BufReader<R>) -> Result<Option<String>>
where
    R: AsyncRead + Unpin,
{
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .await
        .map_err(|e| Error::InternalError(e.to_string()))?;
    if read == 0 {
        return Ok(None);
    }
    while line.ends_with(['\r', '\n']) {
        line.pop();
    }
    Ok(Some(line))
}

fn split_command(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
    Some((command.to_ascii_uppercase(), rest.trim()))
}

/// Parses `FROM:<addr>` / `TO:<addr>` envelope arguments, case-insensitively on
/// the prefix, tolerating an optional space before `<addr>`.
fn parse_address(rest: &str, prefix: &str) -> Option<Address> {
    let rest = rest.trim();
    // `get` (rather than slicing directly) avoids panicking on a UTF-8 boundary
    // if a malformed client sends multi-byte characters before the prefix.
    let head = rest.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let without_prefix = &rest[prefix.len()..];
    let email = without_prefix
        .trim()
        .trim_start_matches('<')
        .split(['>', ' '])
        .next()?
        .trim();
    if email.is_empty() {
        return None;
    }
    Some(Address::new(email))
}

async fn read_data<R>(reader: &mut BufReader<R>, max_message_bytes: usize) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut raw = Vec::new();
    let mut line = Vec::new();
    let mut too_large = false;
    loop {
        let byte = match reader.read_u8().await {
            Ok(byte) => byte,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(Error::InternalError(error.to_string())),
        };

        // Once the limit is exceeded, retaining three bytes is sufficient to
        // recognize the SMTP terminator without buffering the rejected body.
        let line_limit = if too_large {
            3
        } else {
            max_message_bytes.saturating_add(3)
        };
        if line.len() < line_limit {
            line.push(byte);
        } else {
            too_large = true;
            raw.clear();
        }

        if byte == b'\n' {
            if line.ends_with(b"\n") {
                line.pop();
            }
            if line.ends_with(b"\r") {
                line.pop();
            }
            if line == b"." {
                break;
            }
            if !too_large {
                let unescaped = line.strip_prefix(b".").unwrap_or(&line);
                let added = unescaped.len().saturating_add(1);
                if raw.len().saturating_add(added) > max_message_bytes {
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
    if too_large {
        Err(Error::InvalidRequest(format!(
            "message exceeds the {max_message_bytes}-byte emulator limit"
        )))
    } else {
        Ok(raw)
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
    let from = transaction
        .from
        .as_ref()
        .filter(|value| !value.email.is_empty())
        .cloned()
        .unwrap_or(from);

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
        subject,
        headers,
        body_text: Some(body.trim().to_string()),
        body_html: None,
        attachments: Vec::new(),
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
    async fn should_reject_data_when_no_recipient_was_given() {
        let mail = temp_store();
        let (client, server) = tokio::io::duplex(4096);

        let session = tokio::spawn(handle_session(server, mail.clone(), 4096));
        let mut client_reader = ClientBufReader::new(client);

        let mut greeting = String::new();
        client_reader.read_line(&mut greeting).await.unwrap();

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
        let parsed = parse_address("from: <sender@example.com>", "FROM:").expect("should parse");
        assert_eq!(parsed.email, "sender@example.com");

        let parsed = parse_address("TO:<alice@example.com>", "TO:").expect("should parse");
        assert_eq!(parsed.email, "alice@example.com");

        assert!(parse_address("TO:", "TO:").is_none());
    }
}
