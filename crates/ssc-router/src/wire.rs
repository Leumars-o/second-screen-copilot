//! Incremental decoders for streamed HTTP bodies: Server-Sent Events (Groq, Gemini, Anthropic,
//! OpenAI-compatible) and newline-delimited JSON (Ollama). Bytes arrive in arbitrary chunks, so
//! both decoders buffer partial lines (and partial UTF-8 sequences).

/// One SSE event: the `event:` name (if any) and the joined `data:` lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes; returns every event completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            self.line(&line, &mut out);
        }
        out
    }

    /// End of body: dispatch a final event that had no trailing blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&rest).trim_end_matches('\r').to_string();
            self.line(&line, &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => self.data.push(value.to_string()),
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event = None;
            return;
        }
        out.push(SseEvent { event: self.event.take(), data: std::mem::take(&mut self.data).join("\n") });
    }
}

/// Splits a byte stream into complete, non-empty lines.
#[derive(Debug, Default)]
pub struct NdjsonDecoder {
    buf: Vec<u8>,
}

impl NdjsonDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let s = String::from_utf8_lossy(&line).trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
        }
        out
    }

    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.buf);
        let s = String::from_utf8_lossy(&rest).trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_split_across_chunks_crlf_and_comments() {
        let mut d = SseDecoder::new();
        assert!(d.push(b": keep-alive\r\n\r\ndata: {\"a\"").is_empty());
        let ev = d.push(b":1}\r\n\r\nevent: ping\ndata: x\ndata: y\n\n");
        assert_eq!(
            ev,
            vec![
                SseEvent { event: None, data: "{\"a\":1}".into() },
                SseEvent { event: Some("ping".into()), data: "x\ny".into() }
            ]
        );
        assert!(d.push(b"data: [DONE]").is_empty());
        assert_eq!(d.finish(), vec![SseEvent { event: None, data: "[DONE]".into() }]);
    }

    #[test]
    fn sse_utf8_split_mid_char() {
        let mut d = SseDecoder::new();
        let bytes = "data: naïra ₦\n\n".as_bytes();
        let (a, b) = bytes.split_at(9); // inside 'ï'
        assert!(d.push(a).is_empty());
        assert_eq!(d.push(b)[0].data, "naïra ₦");
    }

    #[test]
    fn ndjson_lines() {
        let mut d = NdjsonDecoder::new();
        assert!(d.push(b"{\"a\":").is_empty());
        assert_eq!(d.push(b"1}\n\n{\"b\":2}\n{\"c\""), vec!["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(d.push(b":3}"), Vec::<String>::new());
        assert_eq!(d.finish().as_deref(), Some("{\"c\":3}"));
    }
}
