#![cfg(unix)]
use sqrzl_emulator::mail::{ListMessagesParams, MailStore};
use sqrzl_emulator::storage::ObjectStore;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Process {
    child: Child,
    ports: [u16; 3],
    root: std::path::PathBuf,
}
impl Process {
    fn start(drain_ms: u64) -> Self {
        let sockets: Vec<_> = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let ports = std::array::from_fn(|i| sockets[i].local_addr().unwrap().port());
        let root = std::env::temp_dir().join(format!("sqrzl-runtime-{}", uuid::Uuid::new_v4()));
        drop(sockets);
        let child = Command::new(env!("CARGO_BIN_EXE_sqrzl-emulator"))
            .env_clear()
            .env("SQRZL_BLOBS_PATH", &root)
            .env("SQRZL_API_PORT", ports[0].to_string())
            .env("SQRZL_UI_PORT", ports[1].to_string())
            .env("SQRZL_SMTP_PORT", ports[2].to_string())
            .env("SES_CONFIGURATION_SETS", "drain-events")
            .env("SQRZL_MAX_MCP_SESSIONS", "1")
            .env("SQRZL_MAX_SMTP_RECIPIENTS", "4")
            .env("SQRZL_DRAIN_TIMEOUT_MS", drain_ms.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut process = Self { child, ports, root };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "process exited before ready"
            );
            if process
                .ports
                .iter()
                .all(|port| TcpStream::connect(("127.0.0.1", *port)).is_ok())
            {
                break;
            }
            assert!(Instant::now() < deadline, "process never became ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        process
    }
    fn signal(&self) {
        assert!(Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap()
            .success());
    }
    fn wait(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "drain did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn request(&self, method: &str, path: &str, body: &[u8]) -> String {
        http_request(self.ports[0], method, path, body, &[])
    }
    fn connect(&self, port: usize) -> TcpStream {
        let stream = TcpStream::connect(("127.0.0.1", self.ports[port])).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n", body.len()).unwrap();
    for (key, value) in headers {
        write!(stream, "{key}: {value}\r\n").unwrap();
    }
    stream.write_all(b"\r\n").unwrap();
    stream.write_all(body).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}
fn json_body(response: &str) -> serde_json::Value {
    serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
}
fn line(reader: &mut BufReader<TcpStream>) -> String {
    let mut result = String::new();
    reader.read_line(&mut result).unwrap();
    result
}
fn smtp(reader: &mut BufReader<TcpStream>, command: &str, status: &str) {
    reader.get_mut().write_all(command.as_bytes()).unwrap();
    assert!(line(reader).starts_with(status));
}

#[test]
fn should_close_idle_connections_with_a_successful_sigterm_exit() {
    // Arrange
    let mut process = Process::start(1000);
    let _http = process.connect(0);
    let _ui = process.connect(1);
    let _smtp = process.connect(2);
    // Act
    process.signal();
    let status = process.wait();
    // Assert
    assert!(
        status.success(),
        "SIGTERM did not gracefully terminate: {status}"
    );
}

#[test]
fn should_finish_admitted_upload_before_releasing_storage_ownership() {
    // Arrange
    let mut process = Process::start(2000);
    assert!(process
        .request("PUT", "/drain", b"")
        .starts_with("HTTP/1.1 200"));
    let mut reader = BufReader::new(process.connect(0));
    reader.get_mut().write_all(b"PUT /drain/value HTTP/1.1\r\nHost: localhost\r\nContent-Length: 12\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").unwrap();
    assert!(line(&mut reader).contains("100 Continue"));
    assert_eq!(line(&mut reader), "\r\n");
    reader.get_mut().write_all(b"abcde").unwrap();
    // Act
    process.signal();
    std::thread::sleep(Duration::from_millis(100));
    let ownership = sqrzl_emulator::storage::StorageRootWriter::acquire(&process.root);
    reader.get_mut().write_all(b"fghijkl").unwrap();
    let mut response = String::new();
    reader.read_to_string(&mut response).unwrap();
    let status = process.wait();
    // Assert
    assert!(
        ownership.is_err(),
        "root lock released while upload was still active"
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(status.success());
    let storage = sqrzl_emulator::storage::FilesystemStorage::open(&process.root).unwrap();
    assert_eq!(
        storage.get_object("drain", "value").unwrap().data,
        b"abcdefghijkl"
    );
}

#[test]
fn should_finish_smtp_data_during_sigterm_drain() {
    // Arrange
    let mut process = Process::start(2000);
    let mut reader = BufReader::new(process.connect(2));
    assert!(line(&mut reader).starts_with("220"));
    smtp(&mut reader, "EHLO local\r\n", "250");
    smtp(&mut reader, "MAIL FROM:<sender@example.test>\r\n", "250");
    smtp(&mut reader, "RCPT TO:<receiver@example.test>\r\n", "250");
    smtp(&mut reader, "DATA\r\n", "354");
    reader
        .get_mut()
        .write_all(b"Subject: drain\r\n\r\nstarted")
        .unwrap();
    // Act
    process.signal();
    std::thread::sleep(Duration::from_millis(100));
    reader
        .get_mut()
        .write_all(b" and finished\r\n.\r\n")
        .unwrap();
    let acknowledgement = line(&mut reader);
    let status = process.wait();
    // Assert
    assert!(acknowledgement.starts_with("250"), "{acknowledgement}");
    assert!(status.success());
    let store = sqrzl_emulator::mail::FilesystemMailStore::open(&process.root).unwrap();
    assert_eq!(
        store
            .list_messages("receiver@example.test", ListMessagesParams::default())
            .unwrap()
            .messages
            .len(),
        1
    );
}

#[test]
fn should_cancel_stalled_data_at_the_drain_deadline_without_capturing_partial_mail() {
    // Arrange
    let mut process = Process::start(100);
    let mut reader = BufReader::new(process.connect(2));
    assert!(line(&mut reader).starts_with("220"));
    smtp(&mut reader, "EHLO local\r\n", "250");
    smtp(&mut reader, "MAIL FROM:<sender@example.test>\r\n", "250");
    smtp(&mut reader, "RCPT TO:<receiver@example.test>\r\n", "250");
    smtp(&mut reader, "DATA\r\n", "354");
    reader
        .get_mut()
        .write_all(b"Subject: never finished\r\n")
        .unwrap();
    // Act
    process.signal();
    let status = process.wait();
    // Assert
    assert!(status.success());
    let store = sqrzl_emulator::mail::FilesystemMailStore::open(&process.root).unwrap();
    assert!(store
        .list_messages("receiver@example.test", ListMessagesParams::default())
        .unwrap()
        .messages
        .is_empty());
}

#[test]
fn should_keep_diagnostics_available_when_smtp_consumes_the_work_budget() {
    // Arrange
    let mut process = Process::start(100);
    assert!(process
        .request("PUT", "/drain", b"")
        .starts_with("HTTP/1.1 200"));
    let sessions: Vec<_> = (0..4)
        .map(|_| {
            let mut reader = BufReader::new(process.connect(2));
            assert!(line(&mut reader).starts_with("220"));
            smtp(&mut reader, "EHLO local\r\n", "250");
            smtp(&mut reader, "MAIL FROM:<sender@example.test>\r\n", "250");
            smtp(&mut reader, "RCPT TO:<receiver@example.test>\r\n", "250");
            smtp(&mut reader, "DATA\r\n", "354");
            reader
        })
        .collect();
    // Act
    let health = process.request("GET", "/healthz", b"");
    let overloaded = process.request("PUT", "/drain/not-published", b"not stored");
    let mut metrics = process.connect(1);
    metrics
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut diagnostics = String::new();
    metrics.read_to_string(&mut diagnostics).unwrap();
    process.signal();
    let status = process.wait();
    drop(sessions);
    // Assert
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.contains("\"active_requests\":4"));
    assert!(overloaded.starts_with("HTTP/1.1 503"), "{overloaded}");
    assert!(overloaded.contains("SlowDown"));
    assert!(diagnostics.contains("sqrzl_active_requests 4"));
    assert!(diagnostics.contains("sqrzl_rejected_requests_total 1"));
    assert!(status.success());
    let storage = sqrzl_emulator::storage::FilesystemStorage::open(&process.root).unwrap();
    assert!(storage.get_object("drain", "not-published").is_err());
}

#[test]
fn should_report_degraded_mail_readiness_without_recreating_the_failed_subsystem() {
    // Arrange
    let mut process = Process::start(1000);
    std::fs::rename(process.root.join("_mail"), process.root.join("_mail.saved")).unwrap();
    std::fs::write(process.root.join("_mail"), b"not a directory").unwrap();
    // Act
    let response = process.request("GET", "/healthz", b"");
    process.signal();
    let status = process.wait();
    // Assert
    assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    assert!(response.contains("\"mail_ready\":false"));
    assert!(response.contains("\"sms_ready\":true"));
    assert!(process.root.join("_mail").is_file());
    assert!(status.success());
}

#[test]
fn should_finish_an_admitted_ses_callback_before_sigterm_exit() {
    // Arrange
    let mut process = Process::start(2000);
    let receiver = TcpListener::bind("127.0.0.1:0").unwrap();
    let receiver_port = receiver.local_addr().unwrap().port();
    let (arrived, arrival) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let receiver_thread = std::thread::spawn(move || {
        let mut stream = BufReader::new(receiver.accept().unwrap().0);
        let mut length = 0;
        loop {
            let header = line(&mut stream);
            if header == "\r\n" {
                break;
            }
            if let Some((key, value)) = header.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
        }
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).unwrap();
        arrived.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
        stream
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
            .unwrap();
    });
    let sent = process.request("POST", "/v2/email/outbound-emails", serde_json::json!({
        "FromEmailAddress":"sender@example.test", "Destination":{"ToAddresses":["receiver@example.test"]},
        "ConfigurationSetName":"drain-events", "Content":{"Simple":{"Subject":{"Data":"drain callback"},"Body":{"Text":{"Data":"body"}}}}
    }).to_string().as_bytes());
    assert!(sent.starts_with("HTTP/1.1 200"), "{sent}");
    let id = json_body(&sent)["MessageId"].as_str().unwrap().to_string();
    let destination = http_request(process.ports[1], "PUT", "/admin/v1/ses-event-destinations/drain-events", serde_json::json!({"callback_url":format!("http://127.0.0.1:{receiver_port}/events"),"event_types":["Delivery"]}).to_string().as_bytes(), &[]);
    assert!(destination.starts_with("HTTP/1.1 200"), "{destination}");
    let ui_port = process.ports[1];
    let simulated = std::thread::spawn(move || {
        http_request(ui_port, "POST", "/admin/v1/ses-events", serde_json::json!({"message_id":id,"event_type":"Delivery","request_id":"shutdown-callback"}).to_string().as_bytes(), &[])
    });
    arrival.recv_timeout(Duration::from_secs(3)).unwrap();
    // Act
    process.signal();
    std::thread::sleep(Duration::from_millis(100));
    let during_callback = process.child.try_wait().unwrap();
    release.send(()).unwrap();
    let result = simulated.join().unwrap();
    receiver_thread.join().unwrap();
    let status = process.wait();
    // Assert
    assert!(
        during_callback.is_none(),
        "process exited before callback acknowledgement"
    );
    assert!(result.starts_with("HTTP/1.1 201"), "{result}");
    assert_eq!(json_body(&result)["state"], "completed");
    assert!(status.success());
    let store = sqrzl_emulator::mail::FilesystemMailStore::open(&process.root).unwrap();
    let event = store
        .get_ses_event(json_body(&result)["event_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        serde_json::to_value(event).unwrap()["callback"]["state"],
        "succeeded"
    );
}

#[test]
fn should_reject_mcp_initialization_when_the_retained_session_budget_is_exhausted() {
    // Arrange
    let mut process = Process::start(1000);
    let initialize = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"drain-test","version":"1"}}}).to_string();
    let first = http_request(process.ports[1], "POST", "/mcp", initialize.as_bytes(), &[]);
    assert!(first.starts_with("HTTP/1.1 200"), "{first}");
    assert!(
        first.to_ascii_lowercase().contains("mcp-session-id:"),
        "{first}"
    );
    // Act
    let rejected = http_request(process.ports[1], "POST", "/mcp", initialize.as_bytes(), &[]);
    process.signal();
    let status = process.wait();
    // Assert
    assert!(rejected.starts_with("HTTP/1.1 503"), "{rejected}");
    assert!(rejected.contains("retry-after: 1"));
    assert!(status.success());
}

#[test]
fn should_reject_excess_smtp_recipients_without_discarding_the_admitted_envelope() {
    // Arrange
    let mut process = Process::start(1000);
    let mut reader = BufReader::new(process.connect(2));
    assert!(line(&mut reader).starts_with("220"));
    smtp(&mut reader, "EHLO local\r\n", "250");
    smtp(&mut reader, "MAIL FROM:<sender@example.test>\r\n", "250");
    for index in 0..4 {
        smtp(
            &mut reader,
            &format!("RCPT TO:<recipient-{index}@example.test>\r\n"),
            "250",
        );
    }
    // Act
    reader
        .get_mut()
        .write_all(b"RCPT TO:<overflow@example.test>\r\n")
        .unwrap();
    let rejected = line(&mut reader);
    smtp(&mut reader, "DATA\r\n", "354");
    smtp(
        &mut reader,
        "Subject: bounded envelope\r\n\r\nbody\r\n.\r\n",
        "250",
    );
    smtp(&mut reader, "QUIT\r\n", "221");
    process.signal();
    let status = process.wait();
    // Assert
    assert!(rejected.starts_with("452"), "{rejected}");
    assert!(status.success());
    let store = sqrzl_emulator::mail::FilesystemMailStore::open(&process.root).unwrap();
    let mailboxes = store.list_mailbox_page(None, 50, None).unwrap();
    assert_eq!(mailboxes.mailboxes.len(), 4);
    assert!(mailboxes
        .mailboxes
        .iter()
        .all(|mailbox| mailbox.address != "overflow@example.test"));
}

#[test]
fn should_retain_acknowledged_smtp_mail_after_forced_process_exit() {
    // Arrange
    let mut process = Process::start(1000);
    let mut reader = BufReader::new(process.connect(2));
    assert!(line(&mut reader).starts_with("220"));
    smtp(&mut reader, "EHLO local\r\n", "250");
    smtp(&mut reader, "MAIL FROM:<sender@example.test>\r\n", "250");
    smtp(&mut reader, "RCPT TO:<crash@example.test>\r\n", "250");
    smtp(&mut reader, "DATA\r\n", "354");
    smtp(
        &mut reader,
        "Subject: acknowledged crash\r\n\r\nretained\r\n.\r\n",
        "250",
    );
    // Act
    process.child.kill().unwrap();
    let status = process.child.wait().unwrap();
    let store = sqrzl_emulator::mail::FilesystemMailStore::open(&process.root).unwrap();
    let messages = store
        .list_messages("crash@example.test", ListMessagesParams::default())
        .unwrap();
    // Assert
    assert!(!status.success());
    assert_eq!(messages.messages.len(), 1);
    assert_eq!(messages.messages[0].message.subject, "acknowledged crash");
    assert!(messages.messages[0]
        .message
        .body_text
        .as_deref()
        .unwrap()
        .contains("retained"));
}

#[test]
fn should_retain_disconnected_mcp_work_until_its_callback_finishes() {
    // Arrange
    let mut process = Process::start(2000);
    let receiver = TcpListener::bind("127.0.0.1:0").unwrap();
    let receiver_port = receiver.local_addr().unwrap().port();
    let (arrived, arrival) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let receiver_thread = std::thread::spawn(move || {
        let mut stream = BufReader::new(receiver.accept().unwrap().0);
        let mut length = 0;
        loop {
            let header = line(&mut stream);
            if header == "\r\n" {
                break;
            }
            if let Some((key, value)) = header.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
        }
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).unwrap();
        arrived.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
        let _ = stream
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
    });
    let sent = process.request("POST", "/v2/email/outbound-emails", serde_json::json!({
        "FromEmailAddress":"sender@example.test", "Destination":{"ToAddresses":["receiver@example.test"]},
        "ConfigurationSetName":"drain-events", "Content":{"Simple":{"Subject":{"Data":"MCP callback"},"Body":{"Text":{"Data":"body"}}}}
    }).to_string().as_bytes());
    let id = json_body(&sent)["MessageId"].as_str().unwrap().to_string();
    let destination = http_request(process.ports[1], "PUT", "/admin/v1/ses-event-destinations/drain-events", serde_json::json!({"callback_url":format!("http://127.0.0.1:{receiver_port}/events"),"event_types":["Delivery"]}).to_string().as_bytes(), &[]);
    assert!(destination.starts_with("HTTP/1.1 200"), "{destination}");
    let initialize = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"disconnect-test","version":"1"}}}).to_string();
    let initialized = http_request(process.ports[1], "POST", "/mcp", initialize.as_bytes(), &[]);
    let session = initialized
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("mcp-session-id")
                .then(|| value.trim().to_string())
        })
        .unwrap();
    let headers = [
        ("Mcp-Session-Id", session.as_str()),
        ("MCP-Protocol-Version", "2025-11-25"),
    ];
    let notified = http_request(
        process.ports[1],
        "POST",
        "/mcp",
        br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        &headers,
    );
    assert!(notified.starts_with("HTTP/1.1 202"), "{notified}");
    let body = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mail_ses_event_simulate","arguments":{"message_id":id,"event_type":"Delivery","request_id":"mcp-disconnected"}}}).to_string();
    let mut stream = process.connect(1);
    write!(stream, "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMcp-Session-Id: {session}\r\nMCP-Protocol-Version: 2025-11-25\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    arrival.recv_timeout(Duration::from_secs(3)).unwrap();
    // Act
    drop(stream);
    std::thread::sleep(Duration::from_millis(100));
    let health = http_request(process.ports[1], "GET", "/healthz", &[], &[]);
    process.signal();
    std::thread::sleep(Duration::from_millis(100));
    let during_callback = process.child.try_wait().unwrap();
    release.send(()).unwrap();
    receiver_thread.join().unwrap();
    let status = process.wait();
    // Assert
    assert!(
        json_body(&health)["runtime"]["active_requests"]
            .as_u64()
            .unwrap()
            >= 1,
        "{health}"
    );
    assert!(
        during_callback.is_none(),
        "process exited before detached tool completed"
    );
    assert!(status.success());
    let store = sqrzl_emulator::mail::FilesystemMailStore::open(&process.root).unwrap();
    let events = store.list_ses_events(Some(&id)).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        serde_json::to_value(&events[0]).unwrap()["callback"]["state"],
        "succeeded"
    );
}
