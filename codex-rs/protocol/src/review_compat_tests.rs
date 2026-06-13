use crate::protocol::EventMsg;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn exited_review_mode_accepts_current_and_nested_payloads() {
    for review_output in [
        serde_json::Value::Null,
        json!({
            "findings": [],
            "overall_correctness": "",
            "overall_explanation": "final review assistant output",
            "overall_confidence_score": 0.0,
        }),
    ] {
        let expected = json!({
            "type": "exited_review_mode",
            "turn_id": "turn-1",
            "item_id": "review-1",
            "review_output": review_output,
        });
        for input in [
            expected.clone(),
            json!({
                "type": "exited_review_mode",
                "turn_id": "turn-1",
                "item_id": "review-1",
                "payload": {"review_output": review_output},
            }),
        ] {
            let event: EventMsg = serde_json::from_value(input).unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), expected);
        }
    }
}

#[test]
fn exited_review_mode_accepts_legacy_unwrapped_output() {
    let review_output = json!({
        "findings": [],
        "overall_correctness": "",
        "overall_explanation": "legacy review output",
        "overall_confidence_score": 0.0,
    });
    let mut input = review_output.clone();
    input["type"] = json!("exited_review_mode");
    let event: EventMsg = serde_json::from_value(input).unwrap();
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        json!({"type": "exited_review_mode", "review_output": review_output})
    );
}
