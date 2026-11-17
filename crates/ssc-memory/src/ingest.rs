//! Doc ingest: PDF / DOCX / Markdown / plain text → text → token-sized chunks with overlap.

use ssc_core::{Result, SscError};
use std::io::Read;

/// Supported upload types, detected from the file name's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Pdf,
    Docx,
    Markdown,
    Text,
}

impl DocKind {
    pub fn from_name(name: &str) -> Option<Self> {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
        Some(match ext.as_str() {
            "pdf" => Self::Pdf,
            "docx" => Self::Docx,
            "md" | "markdown" => Self::Markdown,
            "txt" | "text" => Self::Text,
            _ => return None,
        })
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::Docx => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            Self::Markdown => "text/markdown",
            Self::Text => "text/plain",
        }
    }
}

/// Extracts plain text. Markdown is kept as-is (headings carry useful context for the model).
pub fn extract_text(kind: DocKind, bytes: &[u8]) -> Result<String> {
    let text = match kind {
        DocKind::Text | DocKind::Markdown => String::from_utf8_lossy(bytes).into_owned(),
        DocKind::Docx => docx_text(bytes)?,
        DocKind::Pdf => pdf_text(bytes)?,
    };
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    if text.trim().is_empty() {
        return Err(SscError::Invalid("document contains no extractable text".into()));
    }
    Ok(text)
}

fn pdf_text(bytes: &[u8]) -> Result<String> {
    // pdf-extract can panic on malformed files; a bad upload must not take the agent down.
    let bytes = bytes.to_vec();
    std::panic::catch_unwind(move || pdf_extract::extract_text_from_mem(&bytes))
        .map_err(|_| SscError::Invalid("unreadable PDF".into()))?
        .map_err(|e| SscError::Invalid(format!("unreadable PDF: {e}")))
}

fn docx_text(bytes: &[u8]) -> Result<String> {
    use quick_xml::events::Event;

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| SscError::Invalid(format!("not a DOCX file: {e}")))?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|e| SscError::Invalid(format!("DOCX without word/document.xml: {e}")))?
        .read_to_string(&mut xml)
        .map_err(|e| SscError::Invalid(format!("DOCX read failed: {e}")))?;

    let mut reader = quick_xml::Reader::from_str(&xml);
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event().map_err(|e| SscError::Invalid(format!("DOCX XML: {e}")))? {
            Event::Start(e) if e.name().as_ref() == b"w:t" => in_text = true,
            Event::End(e) => match e.name().as_ref() {
                b"w:t" => in_text = false,
                b"w:p" => out.push('\n'),
                _ => {}
            },
            Event::Empty(e) => match e.name().as_ref() {
                b"w:tab" => out.push('\t'),
                b"w:br" | b"w:cr" => out.push('\n'),
                b"w:p" => out.push('\n'),
                _ => {}
            },
            Event::Text(t) if in_text => {
                out.push_str(&t.unescape().map_err(|e| SscError::Invalid(format!("DOCX XML: {e}")))?);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

// ---------- chunking ----------

/// Chunk sizing in tokens (cl100k). Defaults: ~400-token chunks with ~50 tokens of overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkConfig {
    pub target_tokens: usize,
    pub overlap_tokens: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self { target_tokens: 400, overlap_tokens: 50 }
    }
}

/// A chunk of a document and its token count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub tokens: usize,
}

pub fn count_tokens(s: &str) -> usize {
    tiktoken_rs::cl100k_base_singleton().encode_ordinary(s).len()
}

/// Splits on line breaks and sentence ends; the separator before each piece is kept for re-joining.
fn pieces(text: &str) -> Vec<(char, String)> {
    let mut out = Vec::new();
    for (li, line) in text.split('\n').enumerate() {
        let mut sep = if li == 0 { ' ' } else { '\n' };
        let mut start = 0;
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        for (i, &(pos, c)) in chars.iter().enumerate() {
            let next_ws = chars.get(i + 1).is_none_or(|(_, n)| n.is_whitespace());
            if matches!(c, '.' | '!' | '?') && next_ws {
                let end = pos + c.len_utf8();
                let p = line[start..end].trim();
                if !p.is_empty() {
                    out.push((sep, p.to_string()));
                    sep = ' ';
                }
                start = end;
            }
        }
        let rest = line[start..].trim();
        if !rest.is_empty() {
            out.push((sep, rest.to_string()));
        }
    }
    out
}

/// Splits a piece longer than `max` tokens on word boundaries.
fn split_long(piece: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_tok = 0;
    for word in piece.split_whitespace() {
        let t = count_tokens(word) + usize::from(!cur.is_empty());
        if cur_tok + t > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_tok = 0;
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
        cur_tok += t;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Packs sentences into chunks of about `target_tokens`, repeating up to `overlap_tokens` of trailing
/// sentences at the start of the next chunk.
pub fn chunk_text(text: &str, cfg: ChunkConfig) -> Vec<TextChunk> {
    let target = cfg.target_tokens.max(8);
    let overlap = cfg.overlap_tokens.min(target / 2);
    let mut units: Vec<(char, String, usize)> = Vec::new();
    for (sep, p) in pieces(text) {
        let t = count_tokens(&p);
        if t > target {
            for (i, s) in split_long(&p, target).into_iter().enumerate() {
                let t = count_tokens(&s);
                units.push((if i == 0 { sep } else { ' ' }, s, t));
            }
        } else {
            units.push((sep, p, t));
        }
    }

    let join = |us: &[(char, String, usize)]| {
        let mut s = String::new();
        for (i, (sep, p, _)) in us.iter().enumerate() {
            if i > 0 {
                s.push(*sep);
            }
            s.push_str(p);
        }
        s
    };

    let mut chunks = Vec::new();
    let mut cur: Vec<(char, String, usize)> = Vec::new();
    let mut cur_tok = 0usize;
    // Units in `cur` that were carried over as overlap (a chunk of only overlap is not emitted).
    let mut carried = 0usize;
    for u in units {
        if cur_tok + u.2 > target && cur.len() > carried {
            let text = join(&cur);
            chunks.push(TextChunk { tokens: count_tokens(&text), text });
            // Carry trailing units totalling at most `overlap` tokens.
            let mut keep = 0;
            let mut kept_tok = 0;
            for x in cur.iter().rev() {
                if kept_tok + x.2 > overlap {
                    break;
                }
                kept_tok += x.2;
                keep += 1;
            }
            cur.drain(..cur.len() - keep);
            cur_tok = kept_tok;
            // Never let the overlap push the next chunk over target.
            while !cur.is_empty() && cur_tok + u.2 > target {
                cur_tok -= cur.remove(0).2;
            }
            carried = cur.len();
        }
        cur_tok += u.2;
        cur.push(u);
    }
    if cur.len() > carried {
        let text = join(&cur);
        chunks.push(TextChunk { tokens: count_tokens(&text), text });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_from_name() {
        assert_eq!(DocKind::from_name("Pricing.PDF"), Some(DocKind::Pdf));
        assert_eq!(DocKind::from_name("a.docx"), Some(DocKind::Docx));
        assert_eq!(DocKind::from_name("notes.md"), Some(DocKind::Markdown));
        assert_eq!(DocKind::from_name("x.txt"), Some(DocKind::Text));
        assert_eq!(DocKind::from_name("x.exe"), None);
        assert_eq!(DocKind::from_name("noext"), None);
    }

    #[test]
    fn short_text_is_one_chunk() {
        let c = chunk_text("Hello world. Second sentence.", ChunkConfig::default());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "Hello world. Second sentence.");
        assert!(c[0].tokens > 0);
    }

    #[test]
    fn long_text_chunks_near_target_with_overlap() {
        let text: String = (0..300).map(|i| format!("Sentence number {i} talks about topic {i}. ")).collect();
        let cfg = ChunkConfig::default();
        let chunks = chunk_text(&text, cfg);
        assert!(chunks.len() >= 5, "{} chunks", chunks.len());
        for c in &chunks {
            assert!(c.tokens <= cfg.target_tokens + 5, "chunk has {} tokens", c.tokens);
        }
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.tokens >= cfg.target_tokens - 30, "chunk too small: {}", c.tokens);
        }
        // Overlap: the last sentence of chunk i starts chunk i+1.
        for w in chunks.windows(2) {
            let last = w[0].text.rsplit(". ").next().unwrap().trim_end_matches('.');
            assert!(w[1].text.contains(last), "no overlap between chunks");
        }
        // Nothing lost: every sentence appears somewhere.
        for i in [0, 150, 299] {
            assert!(chunks.iter().any(|c| c.text.contains(&format!("Sentence number {i} "))));
        }
    }

    #[test]
    fn giant_sentence_is_split_on_words() {
        let text = "word ".repeat(2000);
        let chunks = chunk_text(&text, ChunkConfig::default());
        assert!(chunks.len() >= 4);
        assert!(chunks.iter().all(|c| c.tokens <= 405));
    }

    #[test]
    fn line_breaks_are_kept() {
        let c = chunk_text("# Title\nFirst line\nSecond line", ChunkConfig::default());
        assert_eq!(c[0].text, "# Title\nFirst line\nSecond line");
    }

    #[test]
    fn txt_extraction_strips_bom_and_rejects_empty() {
        assert_eq!(extract_text(DocKind::Text, "\u{feff}hi\r\nthere".as_bytes()).unwrap(), "hi\nthere");
        assert!(matches!(extract_text(DocKind::Text, b"  \n "), Err(SscError::Invalid(_))));
    }

    #[test]
    fn garbage_pdf_and_docx_are_invalid_not_panics() {
        assert!(matches!(extract_text(DocKind::Pdf, b"not a pdf"), Err(SscError::Invalid(_))));
        assert!(matches!(extract_text(DocKind::Docx, b"not a zip"), Err(SscError::Invalid(_))));
    }
}
