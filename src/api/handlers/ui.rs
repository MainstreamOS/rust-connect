//! UI handlers
//!
//! Single Responsibility: Serve the web dashboard UI.

use axum::http::header;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};

const UI_HTML: &str = include_str!("../ui/index.html");

pub async fn serve_ui() -> impl IntoResponse {
    Html(UI_HTML)
}

pub async fn redirect_to_ui() -> impl IntoResponse {
    (StatusCode::FOUND, [(header::LOCATION, "/ui/")])
}

#[cfg(test)]
mod tests {
    use super::UI_HTML;

    /// Every plugin event the dashboard reacts to must be one the server sends.
    #[test]
    fn the_dashboard_listens_for_events_the_server_sends() {
        let names: Vec<&str> = UI_HTML
            .split("eventType === '")
            .skip(1)
            .filter_map(|rest| rest.split('\'').next())
            .collect();
        assert!(names.contains(&"SmsMessagesReceived"));
        for name in names {
            let parsed = serde_json::from_value::<crate::plugins::PluginEvent>(
                serde_json::json!({ "type": name }),
            );
            if let Err(error) = parsed {
                assert!(
                    !error.to_string().contains("unknown variant"),
                    "the dashboard waits for {name}, which the server never sends"
                );
            }
        }
    }
}
