//! Rust embedding entry point for quickjs-oxide.
pub use quickjs_oxide_engine::*;

/// A runtime configured with default native services or explicit host services.
#[derive(Clone)]
pub struct Runtime(quickjs_oxide_engine::Runtime);
impl Runtime {
    pub fn into_engine(self) -> quickjs_oxide_engine::Runtime {
        self.0
    }
    #[must_use]
    pub fn new() -> Self {
        Self::new_with_host_services(quickjs_oxide_host::SystemHostServices::default())
    }
    #[must_use]
    pub fn new_with_host_services(services: impl HostServices + 'static) -> Self {
        Self(quickjs_oxide_engine::Runtime::new_with_host_services(
            services,
        ))
    }
}
impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
impl std::ops::Deref for Runtime {
    type Target = quickjs_oxide_engine::Runtime;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
