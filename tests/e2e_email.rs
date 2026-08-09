//! Real-socket round trip for the SMTP capture server, proving the listener
//! wiring itself (not just the in-process session state machine already covered
//! by `sqrzl_emulator::mail::smtp`'s unit tests).

use sqrzl_emulator::mail::{FilesystemMailStore, ListMessagesParams, MailStore, SmtpServer};
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};

fn reserve_port() -> u16 {
    let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("port reservation should bind");
    listener
        .local_addr()
        .expect("listener should have local addr")
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn should_capture_message_when_sending_over_a_real_smtp_socket() {
    let port = reserve_port();
    let storage_dir =
        std::env::temp_dir().join(format!("sqrzl-e2e-email-{}", uuid::Uuid::new_v4()));
    let mail = Arc::new(FilesystemMailStore::open(&storage_dir).expect("mail store should open"));
    let mail_for_server = mail.clone();

    tokio::spawn(async move {
        let _ = SmtpServer::new(mail_for_server, port).start().await;
    });

    // Give the listener a moment to bind before connecting.
    sleep(Duration::from_millis(50)).await;

    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("should connect to smtp server");
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let mut greeting = String::new();
    reader
        .read_line(&mut greeting)
        .await
        .expect("should read greeting");
    assert!(greeting.starts_with("220"));

    for line in [
        "EHLO client.example.com",
        "MAIL FROM:<sender@example.com>",
        "RCPT TO:<alice@example.com>",
        "DATA",
    ] {
        writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("write should succeed");
        let mut reply = String::new();
        reader
            .read_line(&mut reply)
            .await
            .expect("read should succeed");
        assert!(
            reply.starts_with("250") || reply.starts_with("354"),
            "unexpected reply to {line:?}: {reply:?}"
        );
    }

    for line in ["Subject: e2e smtp", "", "hello over a real socket", "."] {
        writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("write should succeed");
    }
    let mut reply = String::new();
    reader
        .read_line(&mut reply)
        .await
        .expect("read should succeed");
    assert!(reply.starts_with("250"), "unexpected DATA reply: {reply:?}");

    writer
        .write_all(b"QUIT\r\n")
        .await
        .expect("write should succeed");
    let mut reply = String::new();
    reader
        .read_line(&mut reply)
        .await
        .expect("read should succeed");
    assert!(reply.starts_with("221"));

    let result = mail
        .list_messages("alice@example.com", ListMessagesParams::default())
        .expect("list should succeed");
    assert_eq!(result.messages.len(), 1);
    assert_eq!(result.messages[0].message.subject, "e2e smtp");
}
