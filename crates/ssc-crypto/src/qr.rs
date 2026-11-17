//! QR code: the JSON payload (validated against `contracts/protocol/qr.schema.json`) and an SVG
//! rendering for the settings UI.

use ssc_core::protocol::QrPayload;

#[derive(Debug, thiserror::Error)]
pub enum QrError {
    #[error("QR payload violates the schema: {0}")]
    Schema(String),
    #[error("QR encoding failed: {0}")]
    Encode(String),
}

/// Schema-checked compact JSON (the exact text encoded in the QR code).
pub fn payload_json(payload: &QrPayload) -> Result<String, QrError> {
    let v = serde_json::to_value(payload).map_err(|e| QrError::Schema(e.to_string()))?;
    QrPayload::from_value(&v).map_err(|e| QrError::Schema(e.to_string()))?;
    Ok(v.to_string())
}

/// SVG of the QR code (error-correction level M, at least 256 px).
pub fn svg(payload: &QrPayload) -> Result<String, QrError> {
    let json = payload_json(payload)?;
    let code = qrcode::QrCode::with_error_correction_level(json.as_bytes(), qrcode::EcLevel::M)
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
        let s = svg(&sample()).unwrap();
        assert!(s.starts_with("<?xml") && s.contains("<svg"));
    }

    #[test]
    fn rejects_schema_violations() {
        let mut p = sample();
        p.urls = vec!["http://not-ws".into()];
        assert!(matches!(svg(&p), Err(QrError::Schema(_))));
        let mut p = sample();
        p.token = "short".into();
        assert!(matches!(payload_json(&p), Err(QrError::Schema(_))));
    }
}
