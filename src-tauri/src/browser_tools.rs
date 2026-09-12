use serde::{Deserialize, Serialize};

/// One browser-automation tool.
///
/// SINGLE SOURCE OF TRUTH for the browser tool catalog. The MCP server
/// (`get_tools`), the in-app agent (`agent_tools`), the unattended scheduler
/// allowlist, and the automation rate-limit classifier must all derive from
/// [`browser_tools()`] — never from a second hand-maintained list. Adding a
/// tool here exposes it to every surface at once; the flags decide which
/// policy each surface applies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BrowserTool {
  pub name: String,
  pub description: String,
  pub input_schema: serde_json::Value,
  /// Runs immediately in the agent loop (no confirmation card).
  pub read_only: bool,
  /// Counts against the shared automation rate-limit bucket.
  pub automation: bool,
  /// May run in unattended `agent_browser` cron jobs.
  pub unattended: bool,
}

fn tool(
  name: &str,
  description: &str,
  properties: serde_json::Value,
  required: &[&str],
  read_only: bool,
) -> BrowserTool {
  BrowserTool {
    name: name.to_string(),
    description: description.to_string(),
    input_schema: serde_json::json!({
      "type": "object",
      "properties": properties,
      "required": required,
    }),
    read_only,
    // Every browser-interaction tool drives a real browser: it always
    // consumes automation quota and is always eligible for cron.
    automation: true,
    unattended: true,
  }
}

fn profile_id_prop() -> serde_json::Value {
  serde_json::json!({ "type": "string", "description": "The UUID of the running profile" })
}

/// Every browser-interaction tool, in stable order. The catalog is also
/// served to the frontend task editor via the `agent_tool_catalog` command.
pub fn browser_tools() -> Vec<BrowserTool> {
  vec![
    tool(
      "navigate",
      "Navigate a running browser profile to a URL. Waits for the page to fully load before returning.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "url": { "type": "string", "description": "The URL to navigate to" },
      }),
      &["profile_id", "url"],
      false,
    ),
    tool(
      "screenshot",
      "Take a screenshot of the current page in a running browser profile. Returns base64-encoded image.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "format": { "type": "string", "enum": ["png", "jpeg", "webp"], "description": "Image format (default: png)" },
        "quality": { "type": "integer", "description": "Image quality 0-100 for jpeg/webp (default: 80)" },
        "full_page": { "type": "boolean", "description": "Capture the full scrollable page (default: false)" },
      }),
      &["profile_id"],
      true,
    ),
    tool(
      "evaluate_javascript",
      "Execute JavaScript in the context of the current page and return the result. Works with both static and dynamically-generated content. Set wait_for_load=true if the script triggers navigation (e.g., form.submit()).",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "expression": { "type": "string", "description": "JavaScript expression to evaluate" },
        "await_promise": { "type": "boolean", "description": "Whether to await the result if it's a Promise (default: false)" },
        "wait_for_load": { "type": "boolean", "description": "Wait for page load after execution, use when the script triggers navigation like form.submit() (default: false)" },
      }),
      &["profile_id", "expression"],
      false,
    ),
    tool(
      "click_element",
      "Click on an element identified by a CSS selector. If the click triggers a page navigation, waits for the new page to load before returning.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "selector": { "type": "string", "description": "CSS selector for the element to click" },
      }),
      &["profile_id", "selector"],
      false,
    ),
    tool(
      "type_text",
      "Focus an element by CSS selector and type text into it. By default uses realistic human-like typing with variable speed, natural errors, and self-corrections. Only set instant=true when you are certain the target does not have bot detection (e.g. browser address bars, developer tools, internal apps) — using instant on public websites risks the profile being flagged as a bot.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "selector": { "type": "string", "description": "CSS selector for the input element" },
        "text": { "type": "string", "description": "Text to type into the element" },
        "clear_first": { "type": "boolean", "description": "Clear the input before typing (default: true)" },
        "instant": { "type": "boolean", "description": "Paste all text at once instead of human typing. WARNING: only use on targets without bot detection — using this on public websites risks the profile being flagged." },
        "wpm": { "type": "number", "description": "Target words per minute for human typing (default: 80)" },
      }),
      &["profile_id", "selector", "text"],
      false,
    ),
    tool(
      "get_page_content",
      "Get the content of the current page. Works with both static HTML and JavaScript-rendered content.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "format": { "type": "string", "enum": ["html", "text"], "description": "Content format: 'html' for full HTML, 'text' for visible text only (default: text)" },
        "selector": { "type": "string", "description": "Optional CSS selector to get content of a specific element instead of the whole page" },
      }),
      &["profile_id"],
      true,
    ),
    tool(
      "get_page_info",
      "Get metadata about the current page including URL, title, and readiness state",
      serde_json::json!({
        "profile_id": profile_id_prop(),
      }),
      &["profile_id"],
      true,
    ),
    tool(
      "get_interactive_elements",
      "Enumerate visible interactive elements on the page (buttons, links, inputs, etc.) as a compact indexed list. The returned indices are stable for the current page and can be used with click_by_index and type_by_index instead of guessing CSS selectors. Call this before click_by_index / type_by_index, and re-call after any navigation or major DOM change. Far cheaper in tokens than get_page_content for agentic browsing.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "max_chars": { "type": "integer", "description": "Cap on the serialized output length (default: 40000). The response carries a `truncated` flag if the list was cut off — narrow the viewport or scroll if you need elements past the cutoff." },
      }),
      &["profile_id"],
      true,
    ),
    tool(
      "click_by_index",
      "Click the element at the given index from the last get_interactive_elements call. Indices are valid until the next navigation. If the click triggers navigation, waits for the new page to load before returning.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "index": { "type": "integer", "description": "Zero-based index from the last get_interactive_elements response" },
      }),
      &["profile_id", "index"],
      false,
    ),
    tool(
      "type_by_index",
      "Focus the element at the given index from the last get_interactive_elements call and type text into it. Same human-like-typing defaults as type_text; only set instant=true when you're sure the target lacks bot detection.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "index": { "type": "integer", "description": "Zero-based index from the last get_interactive_elements response" },
        "text": { "type": "string", "description": "Text to type into the element" },
        "clear_first": { "type": "boolean", "description": "Clear the input before typing (default: true)" },
        "instant": { "type": "boolean", "description": "Paste all text at once instead of human typing. WARNING: only use on targets without bot detection." },
        "wpm": { "type": "number", "description": "Target words per minute for human typing (default: 80)" },
      }),
      &["profile_id", "index", "text"],
      false,
    ),
    tool(
      "drag",
      "Drag from a source element to a target element or viewport point. Source and target each resolve from a CSS selector, an index from the last get_interactive_elements call, or explicit x/y coordinates. If the drag triggers navigation, waits for the new page to load before returning.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "from_selector": { "type": "string", "description": "CSS selector for the drag source" },
        "from_index": { "type": "integer", "description": "Index of the drag source from get_interactive_elements" },
        "to_selector": { "type": "string", "description": "CSS selector for the drop target" },
        "to_index": { "type": "integer", "description": "Index of the drop target from get_interactive_elements" },
        "to_x": { "type": "number", "description": "Viewport x coordinate (use with to_y instead of a target)" },
        "to_y": { "type": "number", "description": "Viewport y coordinate (use with to_x instead of a target)" },
      }),
      &["profile_id"],
      false,
    ),
    tool(
      "scroll",
      "Scroll the page or an element. Direction is up, down, left, or right; pixels defaults to 500.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "selector": { "type": "string", "description": "CSS selector of the scrollable element (omit to scroll the page)" },
        "index": { "type": "integer", "description": "Index of the scrollable element from get_interactive_elements" },
        "direction": { "type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction (default: down)" },
        "pixels": { "type": "integer", "description": "Pixels to scroll (default: 500, max 10000)" },
      }),
      &["profile_id"],
      false,
    ),
    tool(
      "press_key",
      "Press a non-text key (Enter, Tab, Escape, Backspace, Delete, arrows, Home, End, PageUp, PageDown). For text entry use type_text instead.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "key": { "type": "string", "description": "Key to press" },
      }),
      &["profile_id", "key"],
      false,
    ),
    tool(
      "hover",
      "Hover the pointer over an element identified by a CSS selector or an index from get_interactive_elements. Useful for revealing menus and tooltips.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "selector": { "type": "string", "description": "CSS selector for the element" },
        "index": { "type": "integer", "description": "Index from the last get_interactive_elements response" },
      }),
      &["profile_id"],
      false,
    ),
    tool(
      "set_download_dir",
      "Route subsequent page downloads into a sandboxed folder. Relative paths resolve under the app downloads root; absolute paths must stay inside the app data directory. Fails when the profile disables agent downloads.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "path": { "type": "string", "description": "Download folder (omit to reset to the profile default)" },
      }),
      &["profile_id"],
      false,
    ),
    tool(
      "wait_for_download",
      "Wait for new files to finish downloading into the current download folder and return them. Skips in-progress partial files.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "timeout_ms": { "type": "integer", "description": "How long to wait in milliseconds (default: 30000, max 300000)" },
      }),
      &["profile_id"],
      false,
    ),
    tool(
      "get_downloads",
      "List finished files in the profile's current download folder.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
      }),
      &["profile_id"],
      true,
    ),
    tool(
      "list_tabs",
      "List open tabs (page targets) of a running profile with their target ids, URLs, and titles. Use the ids with switch_tab and close_tab.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
      }),
      &["profile_id"],
      true,
    ),
    tool(
      "new_tab",
      "Open a new tab in a running profile, optionally at a URL. Returns the new target id.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "url": { "type": "string", "description": "URL to open (omit for a blank tab)" },
      }),
      &["profile_id"],
      false,
    ),
    tool(
      "switch_tab",
      "Bring a tab to the front by target id (see list_tabs).",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "target_id": { "type": "string", "description": "Target id from list_tabs" },
      }),
      &["profile_id", "target_id"],
      false,
    ),
    tool(
      "close_tab",
      "Close a tab by target id (see list_tabs).",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "target_id": { "type": "string", "description": "Target id from list_tabs" },
      }),
      &["profile_id", "target_id"],
      false,
    ),
    tool(
      "wait_for_text",
      "Wait until the page's visible text contains a substring (case-insensitive), or time out.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "text": { "type": "string", "description": "Substring to wait for" },
        "timeout_ms": { "type": "integer", "description": "Wait budget in milliseconds (default: 15000, max 120000)" },
      }),
      &["profile_id", "text"],
      true,
    ),
    tool(
      "wait_for_url",
      "Wait until the current URL contains a substring, or time out.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "contains": { "type": "string", "description": "Substring the URL must contain" },
        "timeout_ms": { "type": "integer", "description": "Wait budget in milliseconds (default: 15000, max 120000)" },
      }),
      &["profile_id", "contains"],
      true,
    ),
    tool(
      "select_option",
      "Select a <select> dropdown option by value or visible text. If the selection triggers navigation, waits for the new page to load.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "selector": { "type": "string", "description": "CSS selector for the <select>" },
        "index": { "type": "integer", "description": "Index from the last get_interactive_elements response" },
        "value": { "type": "string", "description": "Option value or visible text to select" },
      }),
      &["profile_id", "value"],
      false,
    ),
    tool(
      "find_text",
      "Count occurrences of text on the page and return a context snippet around the first match.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "query": { "type": "string", "description": "Text to find" },
      }),
      &["profile_id", "query"],
      true,
    ),
    tool(
      "get_cookies",
      "Read the current page's cookies (name, value, domain, path, expiry). Values are returned so the agent can verify login state.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
      }),
      &["profile_id"],
      true,
    ),
    tool(
      "extract_table",
      "Extract an HTML table into rows of cell text.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
        "selector": { "type": "string", "description": "CSS selector for the <table> (default: first table)" },
      }),
      &["profile_id"],
      true,
    ),
    tool(
      "extract_article",
      "Extract the readable article text of the page (headings and paragraphs, scripts and chrome stripped). Cheaper than get_page_content for reading.",
      serde_json::json!({
        "profile_id": profile_id_prop(),
      }),
      &["profile_id"],
      true,
    ),
  ]
}

/// Look up one catalog tool by name.
pub fn browser_tool(name: &str) -> Option<BrowserTool> {
  browser_tools().into_iter().find(|t| t.name == name)
}

/// True when the catalog marks `name` as read-only (safe to run without
/// confirmation). Non-catalog tools are never read-only by default.
pub fn is_read_only_browser_tool(name: &str) -> bool {
  browser_tool(name).is_some_and(|t| t.read_only)
}

/// True when the catalog marks `name` as automation-budgeted.
pub fn is_automation_browser_tool(name: &str) -> bool {
  browser_tool(name).is_some_and(|t| t.automation)
}

/// Names eligible for unattended `agent_browser` cron execution.
pub fn unattended_browser_tool_names() -> Vec<String> {
  browser_tools()
    .into_iter()
    .filter(|t| t.unattended)
    .map(|t| t.name)
    .collect()
}

/// Serve the catalog to the task editor UI so the frontend never
/// hard-codes a second copy of the tool list.
#[tauri::command]
pub fn agent_tool_catalog() -> Vec<BrowserTool> {
  browser_tools()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn catalog_names_are_unique_and_profile_scoped() {
    let tools = browser_tools();
    assert!(!tools.is_empty());
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), tools.len(), "duplicate tool names");
    for tool in &tools {
      let props = tool.input_schema["properties"]
        .as_object()
        .expect("properties must be an object");
      assert!(
        props.contains_key("profile_id"),
        "{} must take profile_id",
        tool.name
      );
      for required in tool.input_schema["required"].as_array().unwrap_or(&vec![]) {
        let key = required.as_str().unwrap_or_default();
        assert!(
          props.contains_key(key),
          "{} requires {key} but has no such property",
          tool.name
        );
      }
    }
  }

  #[test]
  fn catalog_flags_cover_reads_and_writes() {
    // Spot-check the policy boundary the agent loop depends on.
    for read_only in [
      "screenshot",
      "get_page_content",
      "get_page_info",
      "get_interactive_elements",
      "get_downloads",
      "list_tabs",
      "find_text",
      "get_cookies",
      "extract_table",
      "extract_article",
      "wait_for_text",
      "wait_for_url",
    ] {
      assert!(
        is_read_only_browser_tool(read_only),
        "{read_only} must be read-only"
      );
    }
    for mutating in [
      "navigate",
      "click_element",
      "type_text",
      "new_tab",
      "close_tab",
      "select_option",
    ] {
      assert!(
        !is_read_only_browser_tool(mutating),
        "{mutating} must require confirmation"
      );
    }
    assert!(!is_read_only_browser_tool("no_such_tool"));
    // Unattended cron admits the whole catalog; the scheduler only adds
    // profile-scoped reads (e.g. get_profile_status) on top.
    assert_eq!(unattended_browser_tool_names().len(), browser_tools().len());
  }
}
