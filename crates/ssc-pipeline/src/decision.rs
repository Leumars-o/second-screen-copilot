//! Adapts `ssc_router::DecisionRouter` (Jev with 800 ms timeout, Tev1 failover, network probe)
//! to the `DecisionModel` trait the trigger engine takes.

use async_trait::async_trait;
use ssc_core::Result;
use ssc_core::decision::{Decision, DecisionInput};
use ssc_core::traits::DecisionModel;
use ssc_router::DecisionRouter;
use std::sync::Arc;

pub struct RoutedDecisions(pub Arc<DecisionRouter>);

#[async_trait]
impl DecisionModel for RoutedDecisions {
    fn id(&self) -> &str {
        "decision-router"
    }
    async fn decide(&self, input: &DecisionInput) -> Result<Decision> {
        self.0.decide(input).await.map(|d| d.decision)
    }
}
