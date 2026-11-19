use panda_core::{Call, Event};
use panda_providers::wire::Responses;
use serde_json::json;
#[test]
fn decodes_fragmented_responses_and_preserves_call_identity() {
    let events = [
        json!({"type":"response.output_text.delta","delta":"café"}),
        json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_a","id":"fc_b","name":"read","arguments":"{\"path\":\"hello\"}"}}),
        json!({"type":"response.completed","response":{"status":"completed","usage":{"output_tokens":2}}}),
    ];
    let bytes = events
        .iter()
        .map(|v| format!("data: {v}\r\n\r\n"))
        .collect::<String>();
    for width in 1..=bytes.len() {
        let mut decoder = Responses::default();
        let mut text = String::new();
        let mut usage = None;
        for chunk in bytes.as_bytes().chunks(width) {
            decoder
                .push(chunk, &mut |e| match e {
                    Event::Text(t) => text += &t,
                    Event::Usage(u) => usage = Some(u),
                    _ => {}
                })
                .unwrap();
        }
        let (answer, calls) = decoder.finish().unwrap();
        assert_eq!(answer, "café");
        assert_eq!(text, answer);
        assert_eq!(usage.unwrap()["output_tokens"], 2);
        assert_eq!(
            calls,
            vec![Call {
                id: "call_a".into(),
                name: "read".into(),
                args: json!({"path":"hello"})
            }]
        );
    }
}
#[test]
fn incomplete_response_archives_unparseable_tool_arguments() {
    let events = [
        json!({"type":"response.output_text.delta","delta":"partial response"}),
        json!({"type":"response.output_item.done","item":{"type":"function_call","status":"incomplete","call_id":"c","name":"read","arguments":"{\"path\":"}}),
        json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"max_output_tokens":10,"usage":{"output_tokens":10}}}),
    ];
    let mut decoder = Responses::default();
    for event in events {
        decoder
            .push(format!("data: {event}\n\n").as_bytes(), &mut |_| {})
            .unwrap();
    }
    let cutoff = decoder
        .finish()
        .unwrap_err()
        .downcast::<panda_core::LengthStop>()
        .unwrap();
    assert_eq!(cutoff.text, "partial response");
    assert_eq!(cutoff.tool_fragments[0]["arguments"], "{\"path\":");
    assert_eq!(
        (cutoff.output_tokens, cutoff.output_limit),
        (Some(10), Some(10))
    );
}

#[test]
fn fresh_state_asks_user_to_login_without_an_external_runtime() {
    use panda_core::{Cancel, Provider};
    use panda_providers::subscription::Subscription;
    let dir = tempfile::tempdir().unwrap();
    let id = "openai-codex";
    let mut p = Subscription {
        provider: id.into(),
        model: "test".into(),
        state: dir.path().into(),
    };
    let err = p
        .reply("system", &[], &[], &Cancel::default(), &mut |_| {})
        .unwrap_err();
    assert!(err.to_string().contains(&format!(":login {id}")), "{err}");
}
