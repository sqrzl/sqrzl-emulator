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
    /// Stores a canonical message and any inline media.
    ///
    /// # Errors
    /// Returns an error for invalid data or persistence failures.
    fn store_message(&self, message: NewSmsMessage) -> Result<SmsMessage>;
    /// Gets a message by canonical identifier.
    ///
    /// # Errors
    /// Returns an error when missing or unreadable.
    fn get_message(&self, message_id: &str) -> Result<SmsMessage>;
    /// Gets a message by its provider identifier.
    ///
    /// # Errors
    /// Returns an error when missing or unreadable.
    fn get_message_by_provider_id(&self, provider_message_id: &str) -> Result<SmsMessage>;
    /// Lists messages for one remote peer.
    ///
    /// # Errors
    /// Returns an error for invalid pagination or persistence failures.
    fn list_messages(&self, peer: &str, params: ListSmsParams) -> Result<ListSmsMessagesResult>;
    /// Lists all conversations.
    ///
    /// # Errors
    /// Returns an error when persisted conversation state cannot be read.
    fn list_conversations(&self) -> Result<Vec<SmsConversation>>;
    /// Deletes one message from a peer conversation.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers or persistence failures.
    fn delete_message(&self, peer: &str, message_id: &str) -> Result<()>;
    /// Deletes a conversation and its canonical messages.
    ///
    /// # Errors
    /// Returns an error for invalid peers or persistence failures.
    fn delete_conversation(&self, peer: &str) -> Result<()>;
    /// Applies a terminal delivery transition.
    ///
    /// # Errors
    /// Returns an error for missing messages, invalid transitions, or persistence failures.
    fn transition_delivery(&self, message_id: &str, state: SmsDeliveryState) -> Result<SmsMessage>;
    /// Reads one captured media object.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers or missing media.
    fn read_media(&self, message_id: &str, media_id: &str) -> Result<(SmsMedia, Vec<u8>)>;
    /// Creates or updates a callback destination.
    ///
    /// # Errors
    /// Returns an error for invalid inputs or persistence failures.
    fn put_destination(
        &self,
        provider: SmsProvider,
        local_number: &str,
        callback_url: &str,
    ) -> Result<TextDestination>;
    /// Gets a callback destination.
    ///
    /// # Errors
    /// Returns an error when missing or unreadable.
    fn get_destination(&self, provider: SmsProvider, local_number: &str)
        -> Result<TextDestination>;
    /// Deletes a callback destination.
    ///
    /// # Errors
    /// Returns an error for invalid inputs or persistence failures.
    fn delete_destination(&self, provider: SmsProvider, local_number: &str) -> Result<()>;
    /// Persists an immutable callback attempt.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers, duplicates, or persistence failures.
    fn record_callback(&self, attempt: CallbackAttempt) -> Result<()>;
    /// Gets one callback attempt.
    ///
    /// # Errors
    /// Returns an error when missing or unreadable.
    fn get_callback(&self, attempt_id: &str) -> Result<CallbackAttempt>;
    /// Lists callback attempts for a message.
    ///
    /// # Errors
    /// Returns an error for invalid identifiers or persistence failures.
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
