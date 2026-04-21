pub mod synthetic;

#[cfg(feature = "nvml")]
pub mod nvml;

#[cfg(feature = "dcgm")]
pub mod dcgm;

#[cfg(feature = "cupti")]
pub mod cupti;

use crate::ring::EventRing;
use std::sync::Arc;

/// Common interface for every sampler: given a ring to push into and a stop
/// flag, run until told to stop. All samplers must be cancel-safe (i.e. their
/// destructors release NVML/DCGM/CUPTI handles even on panic).
#[async_trait::async_trait]
pub trait Sampler: Send {
    async fn run(self: Box<Self>, ring: Arc<EventRing>, shutdown: tokio::sync::watch::Receiver<bool>);
}
