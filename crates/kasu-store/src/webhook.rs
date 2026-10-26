use kasu_core::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookStream {
    Native,
    Git,
}
impl WebhookStream {
    pub fn code(self) -> i64 {
        match self {
            Self::Native => 0,
            Self::Git => 1,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookDelivery {
    pub sequence: u64,
    pub body: Vec<u8>,
    pub attempts: u64,
    pub retry_at_ms: i64,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WebhookState {
    pub cursor: u64,
    pub pending: Option<WebhookDelivery>,
}
pub trait WebhookStore: Send + Sync {
    fn webhook_state(
        &self,
        repo: &[u8],
        consumer: &str,
        stream: WebhookStream,
    ) -> Result<WebhookState>;
    /// Install the next body only if the cursor is unchanged and no delivery is
    /// already pending. A concurrent producer receives the authoritative state.
    fn prepare_webhook(
        &self,
        repo: &[u8],
        consumer: &str,
        stream: WebhookStream,
        cursor: u64,
        sequence: u64,
        body: &[u8],
    ) -> Result<WebhookState>;
    /// Acknowledge or postpone only this pending sequence. Stale workers cannot
    /// move the cursor backwards or disturb a newer pending delivery.
    fn finish_webhook(
        &self,
        repo: &[u8],
        consumer: &str,
        stream: WebhookStream,
        sequence: u64,
        retry_at_ms: Option<i64>,
    ) -> Result<bool>;
}
