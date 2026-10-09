//! Minimal durable cancellation decisions, separate from active upload state.

pub(crate) const GCS_XML_CANCELLATION_STATE: &str = "gcs-resumable-cancelled-v1";
pub(crate) const GCS_ACTIVE_SESSION_STATE: &str = "gcs-resumable-session-v2";

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct UploadCancellation {
    pub session_id: String,
    pub bucket: String,
    pub bucket_created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}
