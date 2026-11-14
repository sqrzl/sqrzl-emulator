//! Process admission, connection ownership and coordinated draining.

use serde::Serialize;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RuntimeLimits {
    pub max_connections: usize,
    pub max_requests: usize,
    pub max_mcp_sessions: usize,
    pub max_smtp_recipients: usize,
    pub max_inflight_bytes: usize,
    pub drain_timeout: Duration,
}
impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_connections: 128,
            max_requests: 4,
            max_mcp_sessions: 128,
            max_smtp_recipients: 1000,
            max_inflight_bytes: 512 * 1024 * 1024,
            drain_timeout: Duration::from_secs(30),
        }
    }
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct RuntimeSnapshot {
    pub draining: bool,
    #[serde(skip)]
    drain_deadline: Option<Instant>,
    pub active_mcp_sessions: usize,
    pub active_connections: usize,
    pub active_requests: usize,
    pub active_probes: usize,
    pub reserved_bytes: usize,
    pub rejected_connections: u64,
    pub rejected_requests: u64,
    pub completed_requests: u64,
    pub duration_ms_sum: u64,
    pub status_classes: [u64; 6],
    pub duration_ms_buckets: [u64; 7],
}
#[derive(Debug)]
pub struct RuntimeControl {
    pub limits: RuntimeLimits,
    state: Mutex<RuntimeSnapshot>,
    shutdown: watch::Sender<bool>,
}
impl Default for RuntimeControl {
    fn default() -> Self {
        Self::create(RuntimeLimits::default())
    }
}
impl RuntimeControl {
    fn create(limits: RuntimeLimits) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            limits,
            state: Mutex::default(),
            shutdown,
        }
    }
    #[must_use]
    pub fn new(limits: RuntimeLimits) -> Arc<Self> {
        Arc::new(Self::create(limits))
    }
    fn state(&self) -> std::sync::MutexGuard<'_, RuntimeSnapshot> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.draining = true;
            state.drain_deadline = Some(Instant::now());
            self.shutdown.send_replace(true);
            state
        })
    }
    #[must_use]
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.state().clone()
    }
    pub fn begin_drain(&self) {
        let mut state = self.state();
        if !state.draining {
            state.drain_deadline = Instant::now().checked_add(self.limits.drain_timeout);
        }
        state.draining = true;
        self.shutdown.send_replace(true);
    }
    #[must_use]
    pub fn drain_remaining(&self) -> Duration {
        self.state()
            .drain_deadline
            .map_or(self.limits.drain_timeout, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            })
    }
    pub fn mcp_sessions(&self, count: usize) {
        self.state().active_mcp_sessions = count;
    }
    pub async fn wait_idle(&self) -> bool {
        tokio::time::timeout(self.drain_remaining(), async {
            loop {
                let state = self.snapshot();
                if state.active_requests == 0 && state.active_probes == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.shutdown.subscribe();
        if *receiver.borrow_and_update() {
            return;
        }
        let _ = receiver.wait_for(|stopped| *stopped).await;
    }
    #[must_use]
    pub fn try_connection(self: &Arc<Self>) -> Option<ConnectionGuard> {
        let mut state = self.state();
        if state.draining || state.active_connections >= self.limits.max_connections {
            state.rejected_connections = state.rejected_connections.saturating_add(1);
            return None;
        }
        state.active_connections += 1;
        Some(ConnectionGuard(self.clone()))
    }
    /// One separately bounded diagnostic request remains available under load.
    #[must_use]
    pub fn try_probe(self: &Arc<Self>) -> Option<WorkGuard> {
        let mut state = self.state();
        if state.draining || state.active_probes != 0 {
            return None;
        }
        state.active_probes = 1;
        Some(WorkGuard {
            control: self.clone(),
            bytes: 0,
            started: Instant::now(),
            status: AtomicU16::new(0),
            probe: true,
        })
    }
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let state = self.snapshot();
        let mut text = format!("# TYPE sqrzl_active_connections gauge\nsqrzl_active_connections {}\n# TYPE sqrzl_active_requests gauge\nsqrzl_active_requests {}\nsqrzl_active_probes {}\nsqrzl_reserved_request_bytes {}\nsqrzl_rejected_requests_total {}\nsqrzl_rejected_connections_total {}\nsqrzl_draining {}\n", state.active_connections, state.active_requests, state.active_probes, state.reserved_bytes, state.rejected_requests, state.rejected_connections, u8::from(state.draining));
        for (class, count) in state.status_classes.into_iter().enumerate() {
            let _ = writeln!(
                text,
                "sqrzl_response_status_class_total{{class=\"{class}xx\"}} {count}"
            );
        }
        let mut count = 0u64;
        for (bound, bucket) in ["1", "10", "100", "1000", "10000", "30000", "+Inf"]
            .into_iter()
            .zip(state.duration_ms_buckets)
        {
            count = count.saturating_add(bucket);
            let _ = writeln!(
                text,
                "sqrzl_work_duration_milliseconds_bucket{{le=\"{bound}\"}} {count}"
            );
        }
        let _ = writeln!(text, "sqrzl_work_duration_milliseconds_count {count}\nsqrzl_work_duration_milliseconds_sum {}", state.duration_ms_sum);
        text
    }
    #[must_use]
    pub fn try_request(self: &Arc<Self>, bytes: usize) -> Option<WorkGuard> {
        let mut state = self.state();
        if state.draining
            || state.active_requests >= self.limits.max_requests
            || bytes
                > self
                    .limits
                    .max_inflight_bytes
                    .saturating_sub(state.reserved_bytes)
        {
            state.rejected_requests = state.rejected_requests.saturating_add(1);
            return None;
        }
        state.active_requests += 1;
        state.reserved_bytes += bytes;
        Some(WorkGuard {
            control: self.clone(),
            bytes,
            started: Instant::now(),
            status: AtomicU16::new(0),
            probe: false,
        })
    }
}
/// The same admission lease follows HTTP parts into MCP request workers.
/// Disconnecting a response cannot release capacity held by detached tool work.
#[derive(Clone)]
pub struct WorkLease(Arc<WorkGuard>);
impl From<WorkGuard> for WorkLease {
    fn from(work: WorkGuard) -> Self {
        Self(Arc::new(work))
    }
}
impl WorkLease {
    pub fn status(&self, status: http::StatusCode) {
        self.0.status(status);
    }
}

pub struct ConnectionGuard(Arc<RuntimeControl>);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.active_connections = state.active_connections.saturating_sub(1);
    }
}
pub struct WorkGuard {
    control: Arc<RuntimeControl>,
    bytes: usize,
    started: Instant,
    status: AtomicU16,
    probe: bool,
}
impl WorkGuard {
    pub fn status(&self, status: http::StatusCode) {
        self.status.store(status.as_u16(), Ordering::Relaxed);
    }
}
impl Drop for WorkGuard {
    fn drop(&mut self) {
        let mut state = self.control.state();
        if self.probe {
            state.active_probes = state.active_probes.saturating_sub(1);
        } else {
            state.active_requests = state.active_requests.saturating_sub(1);
        }
        state.reserved_bytes = state.reserved_bytes.saturating_sub(self.bytes);
        state.completed_requests = state.completed_requests.saturating_add(1);
        let class = usize::from(self.status.load(Ordering::Relaxed) / 100).min(5);
        state.status_classes[class] = state.status_classes[class].saturating_add(1);
        let elapsed = self.started.elapsed().as_millis();
        state.duration_ms_sum = state
            .duration_ms_sum
            .saturating_add(u64::try_from(elapsed).unwrap_or(u64::MAX));
        let bucket = [1, 10, 100, 1000, 10_000, 30_000]
            .iter()
            .position(|bound| elapsed <= *bound)
            .unwrap_or(6);
        state.duration_ms_buckets[bucket] = state.duration_ms_buckets[bucket].saturating_add(1);
    }
}
/// Rejects overload before reading a body using the selected provider's error shape.
#[must_use]
pub fn overloaded(provider: Option<&str>, uri: &http::Uri) -> hyper::Response<crate::body::Body> {
    use crate::server::ResponseBuilder;
    use http::StatusCode;
    let builder = ResponseBuilder::new(StatusCode::SERVICE_UNAVAILABLE)
        .header("retry-after", "1")
        .header("connection", "close");
    match provider.unwrap_or("admin") {
        "azure-blob" => builder.content_type("application/xml").header("x-ms-error-code", "ServerBusy")
            .body_str("<Error><Code>ServerBusy</Code><Message>Emulator capacity exhausted or draining</Message></Error>").build(),
        "gcs" if uri.path().starts_with("/storage/") || uri.path().starts_with("/upload/") => builder.content_type("application/json")
            .body(serde_json::json!({"error":{"code":503,"message":"Emulator capacity exhausted or draining","errors":[{"domain":"global","reason":"backendError","message":"Emulator capacity exhausted or draining"}]}}).to_string().into_bytes()).build(),
        "s3" | "s3-family" | "sns" | "gcs" => builder.content_type("application/xml")
            .body_str("<Error><Code>SlowDown</Code><Message>Emulator capacity exhausted or draining</Message></Error>").build(),
        "ses" | "aws-sms-voice-v2" => builder.content_type("application/json").header("x-amzn-errortype", "ServiceUnavailableException")
            .body_str("{\"__type\":\"ServiceUnavailableException\",\"message\":\"Emulator capacity exhausted or draining\"}").build(),
        "oci-object" => builder.content_type("application/json").body_str("{\"code\":\"ServiceUnavailable\",\"message\":\"Emulator capacity exhausted or draining\"}").build(),
        "sendgrid" => builder.content_type("application/json").body_str("{\"errors\":[{\"message\":\"Emulator capacity exhausted or draining\"}]}").build(),
        "acs" => builder.content_type("application/json").body_str("{\"error\":{\"code\":\"ServiceUnavailable\",\"message\":\"Emulator capacity exhausted or draining\"}}").build(),
        "twilio" => builder.content_type("application/json").body_str("{\"code\":20429,\"message\":\"Emulator capacity exhausted or draining\",\"status\":503}").build(),
        _ => builder.content_type("application/json").body_str("{\"code\":\"ServiceUnavailable\",\"error\":\"Emulator capacity exhausted or draining\"}").build(),
    }
}

/// Reaps completed connections continuously; all remaining sessions share a deadline.
pub async fn drain_connections(tasks: &mut tokio::task::JoinSet<()>, timeout: Duration) {
    if tokio::time::timeout(timeout, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_hold_request_capacity_until_response_body_is_dropped() {
        // Arrange
        let control = RuntimeControl::new(RuntimeLimits {
            max_requests: 1,
            ..RuntimeLimits::default()
        });
        let guard = control.try_request(16).unwrap();
        let mut body = crate::body::Body::from("response");
        // Act
        body.retain_work(guard);
        let while_streaming = control.try_request(16);
        drop(body);
        let after_drop = control.try_request(16);
        // Assert
        assert!(while_streaming.is_none());
        assert!(after_drop.is_some());
    }

    #[tokio::test]
    async fn should_reject_new_work_after_drain_without_losing_the_shutdown_notification() {
        // Arrange
        let control = RuntimeControl::new(RuntimeLimits::default());
        let admitted = control.try_request(16).unwrap();
        // Act
        control.begin_drain();
        tokio::time::timeout(std::time::Duration::from_secs(1), control.cancelled())
            .await
            .unwrap();
        // Assert
        assert!(control.try_request(16).is_none());
        assert!(control.try_connection().is_none());
        assert_eq!(control.snapshot().active_requests, 1);
        drop(admitted);
        assert_eq!(control.snapshot().active_requests, 0);
    }

    #[tokio::test]
    async fn should_refund_at_body_eof_but_retain_capacity_for_an_active_worker() {
        // Arrange
        use http_body_util::BodyExt as _;
        let control = RuntimeControl::new(RuntimeLimits::default());
        let lease = WorkLease::from(control.try_request(16).unwrap());
        let worker = lease.clone();
        let mut body = crate::body::Body::from("response");
        body.retain_work(lease);
        // Act
        while body.frame().await.is_some() {}
        let during_worker = control.snapshot();
        drop(worker);
        let after_worker = control.snapshot();
        // Assert
        assert_eq!(during_worker.active_requests, 1);
        assert_eq!(during_worker.reserved_bytes, 16);
        assert_eq!(after_worker.active_requests, 0);
        assert_eq!(after_worker.reserved_bytes, 0);
    }

    #[test]
    fn should_share_one_deadline_across_repeated_drain_signals() {
        // Arrange
        let control = RuntimeControl::new(RuntimeLimits::default());
        // Act
        control.begin_drain();
        let first = control.snapshot().drain_deadline;
        control.begin_drain();
        let second = control.snapshot().drain_deadline;
        // Assert
        assert!(first.is_some());
        assert_eq!(first, second);
    }

    #[test]
    fn should_reject_aggregate_body_reservations_without_queuing() {
        // Arrange
        let control = RuntimeControl::new(RuntimeLimits {
            max_inflight_bytes: 32,
            ..RuntimeLimits::default()
        });
        // Act
        let first = control.try_request(24).unwrap();
        let rejected = control.try_request(24);
        // Assert
        assert!(rejected.is_none());
        assert_eq!(control.snapshot().reserved_bytes, 24);
        drop(first);
        assert!(control.try_request(24).is_some());
    }
}
