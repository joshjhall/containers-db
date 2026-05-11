//! Adapter registry — maps tool id → [`crate::adapter::Adapter`]
//! implementation. Tools without a registered adapter are silently
//! skipped (added to the catalog so the schema validates, but no
//! upstream lookup happens until someone writes the adapter).

use std::collections::HashMap;
use std::sync::Arc;

use crate::adapter::Adapter;

pub mod rust;

/// Built-in registry shipped with the scanner. `register_all` callers
/// can extend this map for tests or future tools.
pub fn register_all() -> HashMap<String, Arc<dyn Adapter>> {
    let mut registry: HashMap<String, Arc<dyn Adapter>> = HashMap::new();
    registry.insert("rust".to_string(), Arc::new(rust::RustAdapter::new()));
    registry
}
