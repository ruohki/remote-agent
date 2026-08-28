//! Help-me mode approval prompt and the on-screen "session active" indicator.
//!
//! Both are abstracted behind traits so the session code can be tested without a GUI:
//! [`NativeApprover`] / [`NativeIndicator`] talk to the platform layer, [`AutoApprover`] and
//! [`NoIndicator`] are the headless variants.

use anyhow::Result;
use protocol::common::OperatorInfo;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    /// Nobody answered within the timeout (treated as a denial).
    TimedOut,
}

#[async_trait::async_trait]
pub trait Approver: Send + Sync + 'static {
    /// Ask the person at the device whether `operator` may connect.
    async fn ask(&self, operator: &OperatorInfo, timeout: Duration) -> Result<ApprovalOutcome>;
}

/// Native modal dialog ("Alice wants to control this computer. Allow?").
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeApprover;

#[async_trait::async_trait]
impl Approver for NativeApprover {
    async fn ask(&self, operator: &OperatorInfo, timeout: Duration) -> Result<ApprovalOutcome> {
        let name = operator.name.clone();
        tokio::task::spawn_blocking(move || crate::platform::approval_dialog(&name, timeout))
            .await?
    }
}

/// Always answers the same thing; used in tests and by the unattended code path.
#[derive(Debug, Clone, Copy)]
pub struct AutoApprover(pub ApprovalOutcome);

#[async_trait::async_trait]
impl Approver for AutoApprover {
    async fn ask(&self, _operator: &OperatorInfo, _timeout: Duration) -> Result<ApprovalOutcome> {
        Ok(self.0)
    }
}

/// Dropping the handle hides the indicator.
pub trait IndicatorHandle: Send + Sync {}

pub trait Indicator: Send + Sync + 'static {
    /// Show a small always-on-top banner naming the operator with a *Disconnect* button;
    /// `on_disconnect` is invoked when the local user presses it.
    fn show(
        &self,
        operator: &OperatorInfo,
        on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn IndicatorHandle>>;
}

/// Platform banner window.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeIndicator;

impl Indicator for NativeIndicator {
    fn show(
        &self,
        operator: &OperatorInfo,
        on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn IndicatorHandle>> {
        crate::platform::show_indicator(&operator.name, on_disconnect)
    }
}

/// Headless variant.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoIndicator;

struct NoHandle;
impl IndicatorHandle for NoHandle {}

impl Indicator for NoIndicator {
    fn show(
        &self,
        _operator: &OperatorInfo,
        _on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn IndicatorHandle>> {
        Ok(Box::new(NoHandle))
    }
}
