//! Client-side fingerprint generation and CDP-based injection for Chromium.
//! Generates random fingerprints matching standard Chromium fingerprint fields,
//! applies geolocation, then injects them via Page.addScriptToEvaluateOnNewDocument
//! and standard CDP domains (Network, Emulation, etc.).

use rand::{Rng, RngExt};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct CdpCommand {
  pub method: String,
  pub params: Value,
}

pub struct FingerprintInjector;

impl FingerprintInjector {
  /// Generate a complete fingerprint JSON for Chromium fingerprint spoofing.
  /// Fields are randomized to produce unique fingerprints per profile.
  pub fn generate_fingerprint(os: &str) -> String {
    let mut rng = rand::rng();
    let (screen_w, screen_h) = Self::random_screen_size(&mut rng, os);
    let (avail_w, avail_h) = (screen_w, screen_h - rng.random_range(30..=80));
    let (window_outer_w, window_outer_h) = (
      screen_w - rng.random_range(0..=200),
      screen_h - rng.random_range(0..=200),
    );

    let device_memory = [0.25f64, 0.5, 1.0, 2.0, 4.0, 8.0][rng.random_range(0..6)];
    let dsf = [1.0f64, 1.25, 1.5, 2.0, 2.5, 3.0][rng.random_range(0..6)];
    let hw_concurrency = rng.random_range(2..=16);
    let color_depth = rng.random_range(24..=48);
    let max_touch = rng.random_range(0..=10);
    let sample_rate = rng.random_range(44100..=48000);
    let channel_count = rng.random_range(1..=2);

    let webgl_params = serde_json::from_str(r#"{
      "UNMASKED_VENDOR_WEBGL": "Google Inc. (Intel)",
      "UNMASKED_RENDERER_WEBGL": "ANGLE (Intel, Intel(R) UHD Graphics Direct3D11 vs_5_0 ps_5_0)",
      "MAX_TEXTURE_SIZE": 16384, "MAX_CUBE_MAP_TEXTURE_SIZE": 16384,
      "MAX_RENDERBUFFER_SIZE": 16384, "MAX_TEXTURE_IMAGE_UNITS": 16,
      "MAX_VERTEX_TEXTURE_IMAGE_UNITS": 16, "MAX_COMBINED_TEXTURE_IMAGE_UNITS": 32,
      "MAX_VERTEX_ATTRIBS": 16, "MAX_VARYING_VECTORS": 30,
      "MAX_VERTEX_UNIFORM_VECTORS": 4096, "MAX_FRAGMENT_UNIFORM_VECTORS": 4096,
      "ALIASED_LINE_WIDTH_RANGE": [1, 1], "ALIASED_POINT_SIZE_RANGE": [1, 1024],
      "MAX_VIEWPORT_DIMS": [32767, 32767]
    }"#).unwrap();
    let webgl2_params = serde_json::from_str(r#"{
      "UNMASKED_VENDOR_WEBGL": "Google Inc. (Intel)",
      "UNMASKED_RENDERER_WEBGL": "ANGLE (Intel, Intel(R) UHD Graphics Direct3D11 vs_5_0 ps_5_0)",
      "MAX_TEXTURE_SIZE": 16384, "MAX_3D_TEXTURE_SIZE": 16384,
      "MAX_ARRAY_TEXTURE_LAYERS": 256, "MAX_COLOR_ATTACHMENTS": 8,
      "MAX_DRAW_BUFFERS": 8, "MAX_SAMPLES": 4
    }"#).unwrap();
    let fonts_val: Value = serde_json::from_str(r#"[
      "Arial","Calibri","Cambria","Cambria Math","Candara",
      "Comic Sans MS","Consolas","Constantia","Corbel","Courier New",
      "Ebrima","Franklin Gothic Medium","Gabriola","Gadugi",
      "Georgia","Impact","Ink Free","Javanese Text","Leelawadee UI",
      "Lucida Console","Lucida Sans Unicode","Malgun Gothic",
      "Microsoft Himalaya","Microsoft JhengHei","Microsoft Sans Serif",
      "Microsoft Tai Le","Microsoft YaHei","Microsoft Yi Baiti",
      "Mongolian Baiti","Myanmar Text","Nirmala UI","Palatino Linotype",
      "Segoe MDL2 Assets","Segoe Print","Segoe Script","Segoe UI",
      "Segoe UI Emoji","Segoe UI Historic","Segoe UI Symbol",
      "SimSun-ExtB","Sitka","Sylfaen","Symbol","Tahoma",
      "Times New Roman","Trebuchet MS","Verdana","Webdings","Wingdings"
    ]"#).unwrap();
    let plugins_val: Value = serde_json::from_str(r#"[
      {"name":"Chrome PDF Plugin","filename":"internal-pdf-viewer","description":"Portable Document Format"},
      {"name":"Chrome PDF Viewer","filename":"mhjfbmdgcfjbbpaeojofohoefgiehjai","description":""},
      {"name":"Native Client","filename":"internal-nacl-plugin","description":""}
    ]"#).unwrap();
    let mime_types_val: Value = serde_json::from_str(r#"[
      "application/pdf","text/pdf","application/x-google-chrome-pdf"
    ]"#).unwrap();

    let mut fp = serde_json::Map::new();
    fp.insert("userAgent".into(), json!(Self::random_ua(&mut rng, os)));
    fp.insert("platform".into(), json!(Self::random_platform(os)));
    fp.insert("hardwareConcurrency".into(), json!(hw_concurrency));
    fp.insert("deviceMemory".into(), json!(device_memory));
    fp.insert("screenWidth".into(), json!(screen_w));
    fp.insert("screenHeight".into(), json!(screen_h));
    fp.insert("screenAvailWidth".into(), json!(avail_w));
    fp.insert("screenAvailHeight".into(), json!(avail_h));
    fp.insert("windowOuterWidth".into(), json!(window_outer_w));
    fp.insert("windowOuterHeight".into(), json!(window_outer_h));
    fp.insert("colorDepth".into(), json!(color_depth));
    fp.insert("pixelDepth".into(), json!(color_depth));
    fp.insert("deviceScaleFactor".into(), json!(dsf));
    fp.insert("language".into(), json!("en-US"));
    fp.insert("languages".into(), json!(["en-US", "en"]));
    fp.insert("timezone".into(), json!("America/New_York"));
    fp.insert("timezoneOffset".into(), json!(300));
    fp.insert("latitude".into(), json!(40.7128));
    fp.insert("longitude".into(), json!(-74.0060));
    fp.insert("webglVendor".into(), json!(rng.random_range(10000..=99999).to_string()));
    fp.insert("webglRenderer".into(), json!(rng.random_range(10000..=99999).to_string()));
    fp.insert("webglParameters".into(), webgl_params);
    fp.insert("webgl2Parameters".into(), webgl2_params);
    fp.insert("fonts".into(), fonts_val);
    fp.insert("plugins".into(), plugins_val);
    fp.insert("mimeTypes".into(), mime_types_val);
    fp.insert("doNotTrack".into(), Value::Null);
    fp.insert("sessionStorage".into(), json!(true));
    fp.insert("localStorage".into(), json!(true));
    fp.insert("indexedDB".into(), json!(true));
    fp.insert("openDatabase".into(), json!(true));
    fp.insert("cpuClass".into(), json!("unknown"));
    fp.insert("oscpu".into(), json!(Self::random_oscpu(os)));
    fp.insert("productSub".into(), json!("20030107"));
    fp.insert("product".into(), json!("Gecko"));
    fp.insert("vendor".into(), json!("Google Inc."));
    fp.insert("vendorSub".into(), json!(""));
    fp.insert("appName".into(), json!("Netscape"));
    fp.insert("appCodeName".into(), json!("Mozilla"));
    fp.insert("appVersion".into(), json!(Self::random_app_version(&mut rng, os)));
    fp.insert("buildID".into(), json!("20181001000000"));
    fp.insert("maxTouchPoints".into(), json!(max_touch));
    fp.insert("cookieEnabled".into(), json!(true));
    fp.insert("onLine".into(), json!(true));
    fp.insert("webdriver".into(), json!(false));
    fp.insert("pdfViewerEnabled".into(), json!(true));
    fp.insert("privateWindow".into(), json!(false));
    fp.insert("audioContext".into(), json!({ "sampleRate": sample_rate, "channelCount": channel_count }));

    serde_json::to_string(&fp).unwrap()
  }

  /// Build CDP commands to apply this fingerprint to a running Chromium instance.
  /// Returns a list of (method, params) to execute via CDP.
  pub fn build_cdp_commands(
    fingerprint_json: &str,
    _proxy_url: Option<&str>,
  ) -> Vec<CdpCommand> {
    let fp: Value = serde_json::from_str(fingerprint_json).unwrap_or_default();
    let mut commands = Vec::new();

    // User agent override (applied to all pages)
    if let Some(ua) = fp.get("userAgent").and_then(|v| v.as_str()) {
      let mut params = json!({ "userAgent": ua });
      if let Some(accept_lang) = fp.get("language").and_then(|v| v.as_str()) {
        params["acceptLanguage"] = json!(accept_lang);
      }
      commands.push(CdpCommand {
        method: "Network.setUserAgentOverride".to_string(),
        params,
      });
    }

    // Device metrics (screen dimensions, device scale factor)
    let screen_w = fp.get("screenWidth").and_then(|v| v.as_u64()).unwrap_or(1920) as i64;
    let screen_h = fp.get("screenHeight").and_then(|v| v.as_u64()).unwrap_or(1080) as i64;
    let dsf = fp.get("deviceScaleFactor").and_then(|v| v.as_f64()).unwrap_or(1.0);
    commands.push(CdpCommand {
      method: "Emulation.setDeviceMetricsOverride".to_string(),
      params: json!({
        "width": screen_w,
        "height": screen_h,
        "deviceScaleFactor": dsf,
        "mobile": false,
        "scale": 1.0,
        "screenWidth": screen_w,
        "screenHeight": screen_h,
        "positionX": 0,
        "positionY": 0,
        "screen": { "width": screen_w, "height": screen_h, "scale": dsf }
      }),
    });

    // Geolocation override (if coordinates exist)
    let lat = fp.get("latitude").and_then(|v| v.as_f64());
    let lng = fp.get("longitude").and_then(|v| v.as_f64());
    if let (Some(lat), Some(lng)) = (lat, lng) {
      commands.push(CdpCommand {
        method: "Emulation.setGeolocationOverride".to_string(),
        params: json!({
          "latitude": lat,
          "longitude": lng,
          "accuracy": 100
        }),
      });
    }

    // Timezone override
    if let Some(tz) = fp.get("timezone").and_then(|v| v.as_str()) {
      commands.push(CdpCommand {
        method: "Emulation.setTimezoneOverride".to_string(),
        params: json!({ "timezoneId": tz }),
      });
    }

    // Locale override
    if let Some(locale) = fp.get("language").and_then(|v| v.as_str()) {
      commands.push(CdpCommand {
        method: "Emulation.setLocaleOverride".to_string(),
        params: json!({ "locale": locale }),
      });
    }

    commands
  }

  /// Generate JavaScript to inject via Page.addScriptToEvaluateOnNewDocument.
  /// This script overrides navigator properties, WebGL, Canvas, AudioContext, etc.
  /// It runs before any page scripts, so the overrides are fully transparent.
  pub fn generate_injection_script(fingerprint_json: &str) -> String {
    let fp: Value = serde_json::from_str(fingerprint_json).unwrap_or_default();

    let platform = fp.get("platform").and_then(|v| v.as_str()).unwrap_or("Win32");
    let hardware_concurrency = fp.get("hardwareConcurrency").and_then(|v| v.as_u64()).unwrap_or(4);
    let device_memory = fp.get("deviceMemory").and_then(|v| v.as_f64()).unwrap_or(8.0);
    let language = fp.get("language").and_then(|v| v.as_str()).unwrap_or("en-US");
    let languages = fp.get("languages").and_then(|v| v.as_array()).map(|a| {
      a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()
    }).unwrap_or_else(|| vec!["en-US", "en"]);
    let max_touch = fp.get("maxTouchPoints").and_then(|v| v.as_u64()).unwrap_or(0);
    let webgl_params = fp.get("webglParameters").and_then(|v| v.as_object());
    let webgl2_params = fp.get("webgl2Parameters").and_then(|v| v.as_object());
    let fonts = fp.get("fonts").and_then(|v| v.as_array()).map(|a| {
      a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()
    }).unwrap_or_default();
    let plugins_json = fp.get("plugins").map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string());
    let mime_types_json = fp.get("mimeTypes").map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string());
    let do_not_track = fp.get("doNotTrack");
    let webdriver = fp.get("webdriver").and_then(|v| v.as_bool()).unwrap_or(false);
    let oscpu = fp.get("oscpu").and_then(|v| v.as_str()).unwrap_or("Windows NT 10.0");
    let app_version = fp.get("appVersion").and_then(|v| v.as_str()).unwrap_or("5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36");
    let cookie_enabled = fp.get("cookieEnabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let on_line = fp.get("onLine").and_then(|v| v.as_bool()).unwrap_or(true);

    // Build WebGL parameter override object
    let webgl_override = webgl_params.map(|obj| {
      let kvs: Vec<String> = obj.iter()
        .map(|(k, v)| format!("p['{}']={};", k, json!(v)))
        .collect();
      kvs.join("")
    }).unwrap_or_default();

    let webgl2_override = webgl2_params.map(|obj| {
      let kvs: Vec<String> = obj.iter()
        .map(|(k, v)| format!("p['{}']={};", k, json!(v)))
        .collect();
      kvs.join("")
    }).unwrap_or_default();

    // Fonts for font enumeration spoofing
    let fonts_json_str = serde_json::to_string(&fonts).unwrap_or_else(|_| "[]".to_string());

    // Build the full injection script
    let dnt_str = match do_not_track {
      Some(Value::Null) => "null".to_string(),
      Some(Value::String(s)) => format!("'{}'", s),
      Some(Value::Bool(b)) => b.to_string(),
      Some(Value::Number(n)) => n.to_string(),
      _ => "null".to_string(),
    };

    let languages_json = serde_json::to_string(&languages).unwrap_or_else(|_| "[]".to_string());

    format!(r#"
// ==UserScript==
// @name        Duckling Fingerprint Injection
// @description Override navigator, screen, WebGL, canvas, and audio properties for anti-detection
// @run-at      document-start
// ==/UserScript==

(function() {{
  'use strict';

  function override(obj, prop, value) {{
    try {{
      Object.defineProperty(obj, prop, {{
        get: function() {{ return value; }},
        configurable: true,
        enumerable: true
      }});
    }} catch(e) {{}}
  }}

  // === Navigator overrides ===
  override(navigator, 'platform', '{platform}');
  override(navigator, 'hardwareConcurrency', {hardware_concurrency});
  override(navigator, 'deviceMemory', {device_memory});
  override(navigator, 'maxTouchPoints', {max_touch});
  override(navigator, 'cookieEnabled', {cookie_enabled});
  override(navigator, 'onLine', {on_line});
  override(navigator, 'webdriver', {webdriver});
  override(navigator, 'oscpu', '{oscpu}');
  override(navigator, 'appVersion', '{app_version}');
  override(navigator, 'buildID', '20181001000000');
  override(navigator, 'productSub', '20030107');
  override(navigator, 'vendor', 'Google Inc.');
  override(navigator, 'vendorSub', '');

  // Language overrides
  override(navigator, 'language', '{language}');
  try {{
    Object.defineProperty(navigator, 'languages', {{
      get: function() {{ return {languages_json}; }},
      configurable: true,
      enumerable: true
    }});
  }} catch(e) {{}}

  // Do Not Track
  try {{
    Object.defineProperty(navigator, 'doNotTrack', {{
      get: function() {{ return {dnt_str}; }},
      configurable: true,
      enumerable: true
    }});
  }} catch(e) {{}}

  // === Screen overrides ===
  var screenW = {screen_w};
  var screenH = {screen_h};
  var availW = {avail_w};
  var availH = {avail_h};
  var colorDepth = {color_depth};
  var pixelDepth = {pixel_depth};

  try {{
    Object.defineProperties(window.screen, {{
      width: {{ get: function() {{ return screenW; }}, configurable: true }},
      height: {{ get: function() {{ return screenH; }}, configurable: true }},
      availWidth: {{ get: function() {{ return availW; }}, configurable: true }},
      availHeight: {{ get: function() {{ return availH; }}, configurable: true }},
      colorDepth: {{ get: function() {{ return colorDepth; }}, configurable: true }},
      pixelDepth: {{ get: function() {{ return pixelDepth; }}, configurable: true }}
    }});
  }} catch(e) {{}}

  // === WebGL spoofing ===
  var origGetParameter = WebGLRenderingContext.prototype.getParameter;
  WebGLRenderingContext.prototype.getParameter = function(p) {{
    var overrides = {{
      {webgl_override}
    }};
    var key = p.toString() + '_' + p;
    if (overrides[key] !== undefined) return overrides[key];
    return origGetParameter.call(this, p);
  }};

  try {{
    var orig2GetParameter = WebGL2RenderingContext.prototype.getParameter;
    WebGL2RenderingContext.prototype.getParameter = function(p) {{
      var overrides = {{
        {webgl2_override}
      }};
      var key = p.toString() + '_' + p;
      if (overrides[key] !== undefined) return overrides[key];
      return orig2GetParameter.call(this, p);
    }};
  }} catch(e) {{}}

  // === Canvas fingerprint spoofing ===
  var canvasSeed = Math.floor(Math.random() * 1000000);
  function addCanvasNoise(data) {{
    for (var i = 0; i < data.length; i += 4) {{
      data[i] = (data[i] + canvasSeed) % 256;
    }}
  }}

  var origToDataURL = HTMLCanvasElement.prototype.toDataURL;
  HTMLCanvasElement.prototype.toDataURL = function() {{
    var ctx = this.getContext && this.getContext('2d');
    if (ctx) {{
      try {{
        var imageData = ctx.getImageData(0, 0, this.width, this.height);
        addCanvasNoise(imageData.data);
        ctx.putImageData(imageData, 0, 0);
      }} catch(e) {{}}
    }}
    return origToDataURL.apply(this, arguments);
  }};

  var origToBlob = HTMLCanvasElement.prototype.toBlob;
  HTMLCanvasElement.prototype.toBlob = function(callback) {{
    var _this = this;
    origToBlob.call(this, function(blob) {{
      // Inject noise before converting
      var ctx = _this.getContext && _this.getContext('2d');
      if (ctx) {{
        try {{
          var imageData = ctx.getImageData(0, 0, _this.width, _this.height);
          addCanvasNoise(imageData.data);
          ctx.putImageData(imageData, 0, 0);
          var canvas2 = document.createElement('canvas');
          canvas2.width = _this.width;
          canvas2.height = _this.height;
          var ctx2 = canvas2.getContext('2d');
          ctx2.putImageData(imageData, 0, 0);
          canvas2.toBlob(callback, arguments[1], arguments[2]);
          return;
        }} catch(e) {{}}
      }}
      callback(blob);
    }}, arguments[1], arguments[2]);
  }};

  // === AudioContext spoofing ===
  var origGetChannelData = AnalyserNode.prototype.getFloatFrequencyData;
  AnalyserNode.prototype.getFloatFrequencyData = function(array) {{
    origGetChannelData.call(this, array);
    for (var i = 0; i < array.length; i++) {{
      array[i] = array[i] + (canvasSeed % 3 - 1) * 0.1;
    }}
  }};

  // === Font spoofing ===
  var spoofedFonts = {fonts_json_str};
  try {{
    var origMeasureText = CanvasRenderingContext2D.prototype.measureText;
    CanvasRenderingContext2D.prototype.measureText = function(text) {{
      var metrics = origMeasureText.call(this, text);
      // Add subtle noise to font metrics
      var width = metrics.width + (canvasSeed % 5) * 0.01;
      return {{
        width: width,
        actualBoundingBoxAscent: metrics.actualBoundingBoxAscent || 0,
        actualBoundingBoxDescent: metrics.actualBoundingBoxDescent || 0,
        fontBoundingBoxAscent: metrics.fontBoundingBoxAscent || 0,
        fontBoundingBoxDescent: metrics.fontBoundingBoxDescent || 0
      }};
    }};
  }} catch(e) {{}}

  // === Plugins spoofing ===
  var pluginsData = {plugins_json};
  try {{
    function PluginArray() {{}}
    PluginArray.prototype = Object.create(Array.prototype);
    PluginArray.prototype.item = function(i) {{ return this[i] || null; }};
    PluginArray.prototype.namedItem = function(name) {{
      for (var i = 0; i < this.length; i++) {{
        if (this[i].name === name) return this[i];
      }}
      return null;
    }};
    var plugins = new PluginArray();
    pluginsData.forEach(function(p) {{ plugins.push(p); }});
    Object.defineProperty(navigator, 'plugins', {{
      get: function() {{ return plugins; }},
      configurable: true
    }});
  }} catch(e) {{}}

  var mimeTypesData = {mime_types_json};
  try {{
    function MimeTypeArray() {{}}
    MimeTypeArray.prototype = Object.create(Array.prototype);
    MimeTypeArray.prototype.item = function(i) {{ return this[i] || null; }};
    MimeTypeArray.prototype.namedItem = function(name) {{
      for (var i = 0; i < this.length; i++) {{
        if (this[i].type === name) return this[i];
      }}
      return null;
    }};
    var mimeTypes = new MimeTypeArray();
    mimeTypesData.forEach(function(m) {{ mimeTypes.push({{type: m, suffixes: '', description: ''}}); }});
    Object.defineProperty(navigator, 'mimeTypes', {{
      get: function() {{ return mimeTypes; }},
      configurable: true
    }});
  }} catch(e) {{}}

  // === WebRTC leak protection ===
  try {{
    var origRTCPeerConnection = window.RTCPeerConnection || window.webkitRTCPeerConnection;
    if (origRTCPeerConnection) {{
      window.RTCPeerConnection = function(config) {{
        var restricted = Object.assign({{}}, config, {{ iceServers: [] }});
        return new origRTCPeerConnection(restricted);
      }};
      window.RTCPeerConnection.prototype = origRTCPeerConnection.prototype;
    }}
  }} catch(e) {{}}

}})();
"#,
      screen_w = fp.get("screenWidth").and_then(|v| v.as_u64()).unwrap_or(1920),
      screen_h = fp.get("screenHeight").and_then(|v| v.as_u64()).unwrap_or(1080),
      avail_w = fp.get("screenAvailWidth").and_then(|v| v.as_u64()).unwrap_or(1920),
      avail_h = fp.get("screenAvailHeight").and_then(|v| v.as_u64()).unwrap_or(1040),
      color_depth = fp.get("colorDepth").and_then(|v| v.as_u64()).unwrap_or(24),
      pixel_depth = fp.get("pixelDepth").and_then(|v| v.as_u64()).unwrap_or(24),
      plugins_json = plugins_json,
      mime_types_json = mime_types_json,
      fonts_json_str = fonts_json_str,
      languages_json = languages_json,
      webgl_override = webgl_override,
      webgl2_override = webgl2_override,
    )
  }

  fn random_screen_size(rng: &mut impl Rng, os: &str) -> (u32, u32) {
    match os {
      "windows" => {
        let pairs = [(1920, 1080), (1366, 768), (1536, 864), (1440, 900), (2560, 1440), (3840, 2160)];
        pairs[rng.random_range(0..pairs.len())]
      }
      "macos" => {
        let pairs = [(1440, 900), (1680, 1050), (2560, 1600), (2880, 1800), (3024, 1964), (3456, 2234)];
        pairs[rng.random_range(0..pairs.len())]
      }
      _ => {
        let pairs = [(1920, 1080), (1366, 768), (1536, 864), (1440, 900)];
        pairs[rng.random_range(0..pairs.len())]
      }
    }
  }

  fn random_ua(rng: &mut impl Rng, os: &str) -> String {
    let chrome_versions = ["136.0.0.0", "135.0.0.0", "134.0.0.0", "133.0.0.0", "132.0.0.0", "131.0.0.0"];
    let cv = chrome_versions[rng.random_range(0..chrome_versions.len())];
    match os {
      "windows" => format!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{cv} Safari/537.36"),
      "macos" => format!("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{cv} Safari/537.36"),
      _ => format!("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{cv} Safari/537.36"),
    }
  }

  fn random_platform(os: &str) -> &'static str {
    match os {
      "windows" => "Win32",
      "macos" => "MacIntel",
      _ => "Linux x86_64",
    }
  }

  fn random_oscpu(os: &str) -> &'static str {
    match os {
      "windows" => "Windows NT 10.0; Win64; x64",
      "macos" => "Intel Mac OS X 10.15",
      _ => "Linux x86_64",
    }
  }

  fn random_app_version(rng: &mut impl Rng, os: &str) -> String {
    let chrome_versions = ["136.0.0.0", "135.0.0.0", "134.0.0.0", "133.0.0.0", "132.0.0.0", "131.0.0.0"];
    let cv = chrome_versions[rng.random_range(0..chrome_versions.len())];
    let safari_vers = ["537.36", "537.35", "537.34"];
    let safari_ver = safari_vers[rng.random_range(0..safari_vers.len())];
    match os {
      "windows" => format!("5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/{safari_ver} (KHTML, like Gecko) Chrome/{cv} Safari/{safari_ver}"),
      "macos" => format!("5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/{safari_ver} (KHTML, like Gecko) Chrome/{cv} Safari/{safari_ver}"),
      _ => format!("5.0 (X11; Linux x86_64) AppleWebKit/{safari_ver} (KHTML, like Gecko) Chrome/{cv} Safari/{safari_ver}"),
    }
  }

}


