//! Web module tests for lecture-distill.
//!
//! Tests the Axum router factory (`create_app`).

use lecture_distill::web::app::create_app;

#[test]
fn test_create_app_returns_router() {
    // create_app is a factory that returns an axum::Router.
    let app = create_app(".");

    // The router should be Send + Sync (axum guarantees this).
    fn assert_send_sync<T: Send + Sync>(_t: &T) {}
    assert_send_sync(&app);
}
