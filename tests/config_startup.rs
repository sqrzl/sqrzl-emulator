use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn should_reject_invalid_runtime_configuration_before_opening_storage() {
    // Arrange
    let cases = [
        ("SQRZL_ACCESS_KEY_ID", "synthetic-id"),
        ("TWILIO_ACCOUNT_SID", "AC00000000000000000000000000000001"),
        ("TWILIO_API_KEY_SECRET", "synthetic-secret"),
        ("SENDGRID_API_KEY", ""),
        ("ACS_CONNECTION_STRING", "not-a-connection-string"),
        ("SQRZL_LIFECYCLE_HOURS", "18446744073709551615"),
    ];
    let mut outcomes = Vec::new();

    // Act
    for (name, value) in cases {
        let root =
            std::env::temp_dir().join(format!("sqrzl-invalid-config-{}", uuid::Uuid::new_v4()));
        let mut child = Command::new(env!("CARGO_BIN_EXE_sqrzl-emulator"))
            .env_clear()
            .env("SQRZL_BLOBS_PATH", &root)
            .env("SQRZL_API_PORT", "0")
            .env("SQRZL_UI_PORT", "0")
            .env("SQRZL_SMTP_PORT", "0")
            .env(name, value)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let exited = child.try_wait().unwrap().is_some();
        if !exited {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        outcomes.push((
            name,
            exited && !output.status.success(),
            String::from_utf8_lossy(&output.stderr).contains(name),
            root.exists(),
        ));
    }

    // Assert
    for (name, rejected, explained, mutated) in outcomes {
        assert!(rejected, "{name} did not fail startup");
        assert!(explained, "{name} did not identify the invalid setting");
        assert!(!mutated, "{name} mutated storage before validation");
    }
}
