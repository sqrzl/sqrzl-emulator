pub mod filesystem;
pub mod model;
pub mod providers;
pub mod simulator;

pub use filesystem::FilesystemSmsStore;
pub use model::{
    CallbackAttempt, CallbackAttemptState, CallbackKind, ListSmsMessagesResult, ListSmsParams,
    NewSmsMedia, NewSmsMessage, SmsChannel, SmsConversation, SmsDeliveryState, SmsDirection,
    SmsMedia, SmsMessage, SmsProvider, TextDestination,
};

use crate::error::Result;

pub trait SmsStore: Send + Sync {
    fn store_message(&self, message: NewSmsMessage) -> Result<SmsMessage>;
    fn get_message(&self, message_id: &str) -> Result<SmsMessage>;
    fn get_message_by_provider_id(&self, provider_message_id: &str) -> Result<SmsMessage>;
    fn list_messages(&self, peer: &str, params: ListSmsParams) -> Result<ListSmsMessagesResult>;
    fn list_conversations(&self) -> Result<Vec<SmsConversation>>;
    fn delete_message(&self, peer: &str, message_id: &str) -> Result<()>;
    fn delete_conversation(&self, peer: &str) -> Result<()>;
    fn transition_delivery(&self, message_id: &str, state: SmsDeliveryState) -> Result<SmsMessage>;
    fn read_media(&self, message_id: &str, media_id: &str) -> Result<(SmsMedia, Vec<u8>)>;
    fn put_destination(
        &self,
        provider: SmsProvider,
        local_number: &str,
        callback_url: &str,
    ) -> Result<TextDestination>;
    fn get_destination(&self, provider: SmsProvider, local_number: &str)
        -> Result<TextDestination>;
    fn delete_destination(&self, provider: SmsProvider, local_number: &str) -> Result<()>;
    fn record_callback(&self, attempt: CallbackAttempt) -> Result<()>;
    fn get_callback(&self, attempt_id: &str) -> Result<CallbackAttempt>;
    fn list_callbacks(&self, message_id: &str) -> Result<Vec<CallbackAttempt>>;
}

#[must_use]
pub fn generate_message_id() -> String {
    format!("txt-{}", uuid::Uuid::new_v4())
}

#[must_use]
pub fn generate_provider_message_id(provider: SmsProvider) -> String {
    match provider {
        SmsProvider::Twilio => format!("SM{}", uuid::Uuid::new_v4().simple()),
        _ => uuid::Uuid::new_v4().to_string(),
    }
}

#[must_use]
pub fn generate_batch_id() -> String {
    format!("batch-{}", uuid::Uuid::new_v4())
}
