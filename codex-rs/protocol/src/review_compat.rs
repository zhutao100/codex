//! Read review events persisted before the review lifecycle payload was standardized.

use crate::protocol::ExitedReviewModeEvent;
use serde::Deserialize;
use serde_json::Value;

pub(crate) fn deserialize_exited_review_mode_event<'de, D>(
    deserializer: D,
) -> Result<ExitedReviewModeEvent, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut value = Value::deserialize(deserializer)?;
    if value.get("review_output").is_none() {
        if let Some(review_output) = value
            .get("payload")
            .and_then(|payload| payload.get("review_output"))
            .cloned()
        {
            value["review_output"] = review_output;
        } else if value.get("findings").is_some() || value.get("overall_confidence_score").is_some()
        {
            return Ok(ExitedReviewModeEvent {
                turn_id: None,
                item_id: None,
                review_output: Some(
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?,
                ),
            });
        }
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
#[path = "review_compat_tests.rs"]
mod tests;
