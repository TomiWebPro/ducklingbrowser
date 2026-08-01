use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single executable browser automation step compiled from an agent's plan.
/// Serialized with a discriminator tag so tasks can be persisted as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum MacroStep {
  Navigate {
    url: String,
  },
  WaitSelector {
    selector: String,
    timeout_ms: Option<u64>,
  },
  Click {
    selector: Option<String>,
    index: Option<u32>,
  },
  Type {
    selector: Option<String>,
    index: Option<u32>,
    text: String,
  },
  Evaluate {
    expression: String,
  },
  Screenshot,
  Extract {
    expression: String,
    key: String,
  },
  SaveProfileField {
    path: String,
    value: Value,
  },
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn macro_step_roundtrips_all_variants() {
    let steps = vec![
      MacroStep::Navigate {
        url: "https://example.com".to_string(),
      },
      MacroStep::WaitSelector {
        selector: "#login".to_string(),
        timeout_ms: Some(10_000),
      },
      MacroStep::Click {
        selector: Some("button.submit".to_string()),
        index: None,
      },
      MacroStep::Type {
        selector: None,
        index: Some(2),
        text: "hello".to_string(),
      },
      MacroStep::Evaluate {
        expression: "document.title".to_string(),
      },
      MacroStep::Screenshot,
      MacroStep::Extract {
        expression: "location.href".to_string(),
        key: "url".to_string(),
      },
      MacroStep::SaveProfileField {
        path: "chromium_config.fingerprint".to_string(),
        value: Value::String("fp".to_string()),
      },
    ];
    let json = serde_json::to_string(&steps).unwrap();
    let back: Vec<MacroStep> = serde_json::from_str(&json).unwrap();
    assert_eq!(steps, back);
  }

  #[test]
  fn macro_step_uses_op_discriminator() {
    let step = MacroStep::Navigate {
      url: "https://example.com".to_string(),
    };
    let value = serde_json::to_value(&step).unwrap();
    assert_eq!(value["op"], "navigate");
    assert_eq!(value["url"], "https://example.com");
  }
}
