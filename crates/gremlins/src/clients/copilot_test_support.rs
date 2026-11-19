//! Process-scoped routing for CLI tests running in isolated subprocesses.

use std::sync::Mutex;

struct Routes {
    exchange: String,
    refresh: String,
    catalog: String,
}

// Credential resolution and exchange run on worker threads; they must see
// the routes installed by the test thread.
static ROUTES: Mutex<Option<Routes>> = Mutex::new(None);

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        ROUTES.lock().unwrap().take();
    }
}

#[must_use = "keep the guard alive while the command is running"]
pub fn with_copilot_test_endpoints(exchange: &str, refresh: &str, catalog: &str) -> impl Drop {
    let mut routes = ROUTES.lock().unwrap();
    assert!(routes.is_none(), "Copilot test routes already installed");
    *routes = Some(Routes {
        exchange: exchange.into(),
        refresh: refresh.into(),
        catalog: catalog.into(),
    });
    Guard
}

pub(crate) fn copilot_test_exchange_url() -> Option<String> {
    ROUTES.lock().unwrap().as_ref().map(|r| r.exchange.clone())
}

pub(crate) fn copilot_test_refresh_url() -> Option<String> {
    ROUTES.lock().unwrap().as_ref().map(|r| r.refresh.clone())
}

pub(crate) fn copilot_test_catalog_base() -> Option<String> {
    ROUTES.lock().unwrap().as_ref().map(|r| r.catalog.clone())
}
