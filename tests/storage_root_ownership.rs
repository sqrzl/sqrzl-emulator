use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Runtime {
    child: Child,
    api_port: u16,
}

impl Runtime {
    fn start(root: &Path) -> Self {
        let listeners = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect::<Vec<_>>();
        let ports = listeners
            .iter()
            .map(|listener| listener.local_addr().unwrap().port())
            .collect::<Vec<_>>();
        drop(listeners);
        let child = Command::new(env!("CARGO_BIN_EXE_sqrzl-emulator"))
            .env("SQRZL_BLOBS_PATH", root)
            .env("SQRZL_API_PORT", ports[0].to_string())
            .env("SQRZL_UI_PORT", ports[1].to_string())
            .env("SQRZL_SMTP_PORT", ports[2].to_string())
            .env("SQRZL_ADMIN_AUTH_DISABLED", "true")
            .env("RUST_LOG", "error")
            .env_remove("SQRZL_ACCESS_KEY_ID")
            .env_remove("SQRZL_SECRET_ACCESS_KEY")
            .env_remove("SQRZL_ACS_CONNECTION_STRING")
            .env_remove("SQRZL_TWILIO_ACCOUNT_SID")
            .env_remove("SQRZL_TWILIO_AUTH_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            child,
            api_port: ports[0],
        }
    }

    fn healthy(&self) -> bool {
        let Ok(mut stream) = TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", self.api_port).parse().unwrap(),
            Duration::from_millis(100),
        ) else {
            return false;
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        if stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .is_err()
        {
            return false;
        }
        let mut response = [0; 64];
        stream
            .read(&mut response)
            .is_ok_and(|size| response[..size].starts_with(b"HTTP/1.1 200"))
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            assert!(self.child.try_wait().unwrap().is_none(), "runtime exited");
            if self.healthy() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("runtime never became healthy");
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TemporaryRoot(std::path::PathBuf);

impl TemporaryRoot {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("sqrzl-root-ownership-{}", uuid::Uuid::new_v4())))
    }
}

impl Drop for TemporaryRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn should_reject_second_runtime_given_one_storage_root_when_writer_is_active() {
    // Arrange
    let root = TemporaryRoot::new();
    let mut first = Runtime::start(&root.0);
    first.wait_ready();

    // Act
    let mut second = Runtime::start(&root.0);
    let deadline = Instant::now() + Duration::from_secs(2);
    let rejected = loop {
        if let Some(status) = second.child.try_wait().unwrap() {
            break !status.success();
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    // Assert
    assert!(rejected, "second runtime can write to the active root");
    let mut error = String::new();
    second
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert!(error.contains("already has an active writer"), "{error}");
    assert!(first.healthy());

    // An abnormal termination releases the OS lock without deleting its inode.
    first.child.kill().unwrap();
    first.child.wait().unwrap();
    let mut reopened = Runtime::start(&root.0);
    reopened.wait_ready();
    assert!(reopened.healthy());
}

#[test]
fn should_allow_independent_runtimes_given_distinct_storage_roots_when_started_together() {
    // Arrange
    let left = TemporaryRoot::new();
    let right = TemporaryRoot::new();

    // Act
    let mut first = Runtime::start(&left.0);
    let mut second = Runtime::start(&right.0);
    first.wait_ready();
    second.wait_ready();

    // Assert
    assert!(first.healthy());
    assert!(second.healthy());
}

#[test]
fn should_preserve_legacy_root_given_startup_owner_when_format_is_unmarked() {
    // Arrange
    let root = TemporaryRoot::new();
    std::fs::create_dir_all(&root.0).unwrap();
    std::fs::write(root.0.join("legacy-data"), b"keep me").unwrap();

    // Act
    let mut runtime = Runtime::start(&root.0);
    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "legacy root was accepted");
        std::thread::sleep(Duration::from_millis(25));
    }

    // Assert
    assert_eq!(
        std::fs::read(root.0.join("legacy-data")).unwrap(),
        b"keep me"
    );
    assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
}
