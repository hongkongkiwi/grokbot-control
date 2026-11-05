//! Live-backend tests against a local stub xAI server (no real key).
//! The stub is shaped from captured request/response shapes; live-only
//! fields stay marked unverified in the README.

use axum::{
    routing::{get, post},
    Json, Router,
};
use grokbot_control::backend::{ChatRequest, Credential, GrokBackend, XaiBackend};
use grokbot_control::state::Message;
use serde_json::{json, Value};

async fn start_stub(mode: &'static str) -> String {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(json!({"data": [{"id": "grok-stub-1"}]})) }),
        )
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| async move {
                assert_eq!(body["model"], "grok-stub-1");
                match mode {
                    "empty" => Json(json!({"id": "x", "choices": []})),
                    "unauthorized" => Json(json!({"error": "bad key"})),
                    _ => Json(json!({
                        "id": "chatcmpl-stub",
                        "model": "grok-stub-1",
                        "choices": [{"message": {"role": "assistant", "content": "stub reply"},
                                     "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 11, "completion_tokens": 7}
                    })),
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://127.0.0.1:{port}/v1")
}

fn backend(base: String) -> XaiBackend {
    XaiBackend::new(Credential::ApiKey("test-key".into()), Some(base)).unwrap()
}

fn req() -> ChatRequest {
    ChatRequest::new(
        "grok-stub-1".into(),
        vec![Message::new("user", "hi")],
        0.7,
        100,
    )
}

#[tokio::test]
async fn stub_chat_models_and_top_p_plumbing() {
    let b = backend(start_stub("ok").await);
    let mut r = req();
    r.top_p = Some(0.5);
    let reply = b.chat(r).await.unwrap();
    assert_eq!(reply.content, "stub reply");
    assert_eq!(reply.prompt_tokens, 11);
    assert_eq!(reply.response_id.as_deref(), Some("chatcmpl-stub"));
    let models = b.list_models().await.unwrap();
    assert_eq!(models[0].id, "grok-stub-1");
    let (kind, _) = b.credential_info();
    assert_eq!(kind, "api_key");
}

#[tokio::test]
async fn stub_empty_choices_is_a_loud_error() {
    let b = backend(start_stub("empty").await);
    let err = b.chat(req()).await.unwrap_err().to_string();
    assert!(err.contains("no choices"), "got: {err}");
}
