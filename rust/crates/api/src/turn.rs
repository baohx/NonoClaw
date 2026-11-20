//! Provider-neutral turn execution. Transports may use HTTP or a supervised CLI.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::{ApiFormat, RequestParams, StreamEvent, StreamFailure, TurnOutput};

#[async_trait]
pub trait TurnClient: Send + Sync {
    async fn run_turn_with_cancel(
        &self,
        params: &RequestParams,
        on_event: &mut (dyn for<'a> FnMut(&'a StreamEvent) + Send),
        cancel: CancellationToken,
    ) -> std::result::Result<TurnOutput, StreamFailure>;

    fn label(&self) -> &'static str;

    /// The HTTP wire format, or `None` for a process transport.
    fn api_format(&self) -> Option<ApiFormat>;

    fn capabilities_for_model(&self, model: &str) -> crate::ProviderCapabilities;

    async fn run_turn(
        &self,
        params: &RequestParams,
        on_event: &mut (dyn for<'a> FnMut(&'a StreamEvent) + Send),
    ) -> std::result::Result<TurnOutput, StreamFailure> {
        self.run_turn_with_cancel(params, on_event, CancellationToken::new())
            .await
    }
}

#[async_trait]
impl TurnClient for crate::Client {
    async fn run_turn_with_cancel(
        &self,
        params: &RequestParams,
        on_event: &mut (dyn for<'a> FnMut(&'a StreamEvent) + Send),
        cancel: CancellationToken,
    ) -> std::result::Result<TurnOutput, StreamFailure> {
        crate::Client::run_turn_with_cancel(self, params, on_event, cancel).await
    }

    fn label(&self) -> &'static str {
        self.api_format().label()
    }

    fn api_format(&self) -> Option<ApiFormat> {
        Some(crate::Client::api_format(self))
    }

    fn capabilities_for_model(&self, model: &str) -> crate::ProviderCapabilities {
        crate::Client::capabilities_for_model(self, model)
    }
}
