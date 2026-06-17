use super::{AppContext, Service};
use anyhow::Result;
use std::sync::Arc;

pub struct Supervisor {
    services: Vec<Box<dyn Service>>,
    #[allow(dead_code)] // services will consume shared context in later steps
    context: Arc<AppContext>,
}

impl Supervisor {
    pub fn new(context: Arc<AppContext>) -> Self {
        Supervisor {
            services: Vec::new(),
            context,
        }
    }

    /// Register a service. Boxing is handled internally for ergonomics.
    pub fn register<S: Service + 'static>(&mut self, service: S) {
        self.services.push(Box::new(service));
    }

    /// Start every registered service in registration order.
    pub async fn start_all(&mut self) -> Result<()> {
        for service in &mut self.services {
            tracing::info!(service = service.name(), "starting service");
            service.start().await?;
        }
        Ok(())
    }

    /// Stop services in reverse registration order so downstream consumers
    /// drain before their upstream producers disappear. Best-effort: a failing
    /// service is logged but does not abort the rest of the shutdown.
    pub async fn stop_all(&mut self) -> Result<()> {
        for service in self.services.iter_mut().rev() {
            tracing::info!(service = service.name(), "stopping service");
            if let Err(e) = service.stop().await {
                tracing::error!(service = service.name(), error = %e, "service stop failed");
            }
        }
        Ok(())
    }
}