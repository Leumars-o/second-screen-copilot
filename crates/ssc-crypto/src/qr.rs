//! QR code: the JSON payload (validated against `contracts/protocol/qr.schema.json`) and an SVG
//! rendering for the settings UI.
//!
//! CR-19: with a phone-app URL the QR text is `<pwa_url>#pair=<base64url(JSON)>`, so a phone's
//! camera opens the app and pairs in one step. The payload rides in the fragment, which browsers
//! never send to the server.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ssc_core::protocol::QrPayload;

#[derive(Debug, thiserror::Error)]
pub enum QrError {
    #[error("QR payload violates the schema: {0}")]
    Schema(String),
    #[error("QR encoding failed: {0}")]
    Encode(String),
    #[error("Phone app URL must start with https:// (http:// only for localhost) and have no #: {0}")]
    PwaUrl(String),
}

/// Accepts `https://…`, or `http://localhost…` / `http://127.0.0.1…` for development; no fragment.
pub fn check_pwa_url(url: &str) -> Result<(), QrError> {
    let ok =
        (url.starts_with("https://") || url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1"))
            && !url.contains('#')
            && !url.contains(char::is_whitespace);
    if ok { Ok(()) } else { Err(QrError::PwaUrl(url.to_string())) }
}

/// The exact text encoded in the QR code: the JSON, or a link to the phone app carrying it.
pub fn qr_text(payload: &QrPayload, pwa_url: Option<&str>) -> Result<String, QrError> {
    let json = payload_json(payload)?;
    match pwa_url {
        None => Ok(json),
        Some(u) => {
            check_pwa_url(u)?;
            Ok(format!("{u}#pair={}", URL_SAFE_NO_PAD.encode(json.as_bytes())))
        }
    }
}

/// Schema-checked compact JSON (the exact text encoded in the QR code).
pub fn payload_json(payload: &QrPayload) -> Result<String, QrError> {
    let v = serde_json::to_value(payload).map_err(|e| QrError::Schema(e.to_string()))?;
    QrPayload::from_value(&v).map_err(|e| QrError::Schema(e.to_string()))?;
    Ok(v.to_string())
}

/// SVG of the QR code (error-correction level M, at least 256 px).
pub fn svg(payload: &QrPayload, pwa_url: Option<&str>) -> Result<String, QrError> {
    let text = qr_text(payload, pwa_url)?;
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M)
        .map_err(|e| QrError::Encode(e.to_string()))?;
    Ok(code.render::<qrcode::render::svg::Color<'_>>().min_dimensions(256, 256).quiet_zone(true).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> QrPayload {
        QrPayload {
            v: 1,
            urls: vec![
                "ws://192.168.1.20:47823".into(),
                "wss://relay.example.com/r/42820c9fc7ad02737ae7dacc15a76d09".into(),
            ],
            pc_pub: "B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9/AsrhtHHw=".into(),
            token: "yMnKy8zNzs/Q0dLT1NXW19jZ2tvc3d7f4OHi4+Tl5uc=".into(),
            exp: 1_763_290_120,
        }
    }

    #[test]
    fn renders_svg() {
        let s = svg(&sample(), None).unwrap();
        assert!(s.starts_with("<?xml") && s.contains("<svg"));
        let s = svg(&sample(), Some("https://app.example.com/")).unwrap();
        assert!(s.contains("<svg"));
    }

    #[test]
    fn link_form_round_trips() {
        let json = payload_json(&sample()).unwrap();
        assert_eq!(qr_text(&sample(), None).unwrap(), json);
        let t = qr_text(&sample(), Some("https://app.example.com/")).unwrap();
        let b64 = t.strip_prefix("https://app.example.com/#pair=").expect("prefix");
        assert!(!b64.contains(['+', '/', '=']), "base64url without padding");
        assert_eq!(URL_SAFE_NO_PAD.decode(b64).unwrap(), json.as_bytes());
    }

    #[test]
    fn rejects_bad_pwa_urls() {
        for bad in ["http://app.example.com/", "https://a.example.com/#x", "app.example.com", "https://a b"] {
            assert!(matches!(qr_text(&sample(), Some(bad)), Err(QrError::PwaUrl(_))), "{bad}");
        }
        for ok in ["https://app.example.com", "http://localhost:3000/", "http://127.0.0.1:4710/"] {
            assert!(check_pwa_url(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_schema_violations() {
        let mut p = sample();
        p.urls = vec!["http://not-ws".into()];
        assert!(matches!(svg(&p, None), Err(QrError::Schema(_))));
        let mut p = sample();
        p.token = "short".into();
        assert!(matches!(payload_json(&p), Err(QrError::Schema(_))));
    }
}
