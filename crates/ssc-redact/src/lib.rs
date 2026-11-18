//! `ssc-redact`: deterministic masking applied to every prompt before it goes to a cloud model
//! (Build Sheet §3 rule 7, §6 W1-MODELS). Plain code, no model involved.
//!
//! What is masked:
//! - **Card numbers**: 13–19 digits, optionally grouped with single spaces or dashes, that pass the
//!   Luhn check → `[CARD]`. When a digit run is longer than a card (e.g. a card followed by another
//!   number), the longest Luhn-valid prefix ending on a group boundary is masked.
//! - **BVN / NIN**: a standalone run of exactly 11 digits → `[ID]`.
//! - **Passwords and secrets**: `password=…`, `pwd: …`, `api_key=…`, `token: …`, `pin=…`,
//!   `"my password is …"`, `"the PIN is …"` → the key is kept, the value becomes `[REDACTED]`.
//! - **Emails** (optional, [`Options::emails`]) → `[EMAIL]`.
//!
//! Known limits (documented, tested where possible):
//! - Nigerian mobile numbers are also 11 digits. A run matching `0[789][01]XXXXXXXX` is treated as a
//!   phone number and left alone **unless** "BVN" or "NIN" appears within the 24 characters before
//!   it. A BVN/NIN that happens to look like a mobile number and has no such label is not masked.
//! - An 11-digit id written with spaces ("2234 5678 901") is not recognised as a BVN/NIN.
//! - Any Luhn-valid 13–19 digit number is masked as a card, so roughly 1 in 10 long order or
//!   account numbers is over-masked. That is the intended trade-off (over-mask, never under-mask).
//! - Secret values end at the first whitespace (or closing quote when quoted), so a password with
//!   spaces is only partly masked unless it is quoted. Prose such as "password is required" masks
//!   the word after "is" (over-masking).
//! - Only ASCII digits are recognised.

use regex::{Captures, Regex};
use ssc_core::types::Prompt;
use std::sync::LazyLock;

pub const CARD_MASK: &str = "[CARD]";
pub const ID_MASK: &str = "[ID]";
pub const SECRET_MASK: &str = "[REDACTED]";
pub const EMAIL_MASK: &str = "[EMAIL]";

/// Redaction switches. Cards, ids and secrets are always on when redaction runs at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    pub emails: bool,
}

/// How many items of each kind were masked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub cards: usize,
    pub ids: usize,
    pub secrets: usize,
    pub emails: usize,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.cards + self.ids + self.secrets + self.emails
    }
    fn add(&mut self, o: Counts) {
        self.cards += o.cards;
        self.ids += o.ids;
        self.secrets += o.secrets;
        self.emails += o.emails;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub counts: Counts,
}

const SECRET_KEYS: &str = r"password|passwd|passphrase|passcode|pwd|pass|pin|otp|secret|client[_-]?secret|api[_-]?key|apikey|access[_-]?token|auth[_-]?token|token";

static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)\b((?:{SECRET_KEYS}))(\s*[:=]\s*|\s+(?:is|was)\s+(?:["']?\s*)?)("[^"]*"|'[^']*'|[^\s"']+)"#
    ))
    .expect("secret regex")
});

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)*\.[A-Za-z]{2,}").expect("email")
});

/// Mask sensitive data in `text`.
pub fn redact(text: &str, opts: Options) -> Redacted {
    let mut counts = Counts::default();

    let text = SECRET_RE
        .replace_all(text, |c: &Captures| {
            counts.secrets += 1;
            format!("{}{}{}", &c[1], &c[2], SECRET_MASK)
        })
        .into_owned();

    let (text, numeric) = redact_numbers(&text);
    counts.add(numeric);

    let text = if opts.emails {
        EMAIL_RE
            .replace_all(&text, |_: &Captures| {
                counts.emails += 1;
                EMAIL_MASK
            })
            .into_owned()
    } else {
        text
    };

    Redacted { text, counts }
}

/// Redact both halves of a prompt. Deterministic, so a byte-stable prefix stays byte-stable.
pub fn redact_prompt(prompt: &Prompt, opts: Options) -> (Prompt, Counts) {
    let a = redact(&prompt.prefix, opts);
    let b = redact(&prompt.dynamic, opts);
    let mut counts = a.counts;
    counts.add(b.counts);
    (Prompt { prefix: a.text, dynamic: b.text, ..prompt.clone() }, counts)
}

/// Standard Luhn check over ASCII digits.
pub fn luhn_valid(digits: &[u8]) -> bool {
    if digits.is_empty() {
        return false;
    }
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = u32::from(d - b'0');
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

/// A maximal run of ASCII digits, byte range `[start, end)`.
#[derive(Debug, Clone, Copy)]
struct Group {
    start: usize,
    end: usize,
}

fn redact_numbers(text: &str) -> (String, Counts) {
    let b = text.as_bytes();
    let mut groups = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let s = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            groups.push(Group { start: s, end: i });
        } else {
            i += 1;
        }
    }

    let mut counts = Counts::default();
    let mut spans: Vec<(usize, usize, &str)> = Vec::new();
    let mut used = vec![false; groups.len()];

    // Sequences: groups joined by exactly one ' ' or '-'.
    let mut g = 0;
    while g < groups.len() {
        let mut last = g;
        while last + 1 < groups.len() {
            let gap = &b[groups[last].end..groups[last + 1].start];
            if gap.len() == 1 && (gap[0] == b' ' || gap[0] == b'-') {
                last += 1;
            } else {
                break;
            }
        }
        // Cards within groups[g..=last]: from each group start, the longest Luhn-valid 13–19 digit span
        // ending on a group boundary.
        let mut s = g;
        while s <= last {
            if !boundary_before(b, groups[s].start) {
                s += 1;
                continue;
            }
            let mut digits: Vec<u8> = Vec::new();
            let mut best = None;
            for e in s..=last {
                digits.extend_from_slice(&b[groups[e].start..groups[e].end]);
                if digits.len() > 19 {
                    break;
                }
                if digits.len() >= 13 && boundary_after(b, groups[e].end) && luhn_valid(&digits) {
                    best = Some(e);
                }
            }
            if let Some(e) = best {
                spans.push((groups[s].start, groups[e].end, CARD_MASK));
                counts.cards += 1;
                for u in used.iter_mut().take(e + 1).skip(s) {
                    *u = true;
                }
                s = e + 1;
            } else {
                s += 1;
            }
        }
        g = last + 1;
    }

    // BVN / NIN: a standalone group of exactly 11 digits.
    for (k, grp) in groups.iter().enumerate() {
        if used[k] || grp.end - grp.start != 11 {
            continue;
        }
        if !boundary_before(b, grp.start) || !boundary_after(b, grp.end) {
            continue;
        }
        if grp.start > 0 && b[grp.start - 1] == b'+' {
            continue;
        }
        let digits = &b[grp.start..grp.end];
        if looks_like_ng_mobile(digits) && !id_label_before(text, grp.start) {
            continue;
        }
        spans.push((grp.start, grp.end, ID_MASK));
        counts.ids += 1;
    }

    if spans.is_empty() {
        return (text.to_string(), counts);
    }
    spans.sort_by_key(|s| s.0);
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for (s, e, mask) in spans {
        out.push_str(&text[pos..s]);
        out.push_str(mask);
        pos = e;
    }
    out.push_str(&text[pos..]);
    (out, counts)
}

/// Not preceded by a letter, digit, '.' or ','-joined digit (so "1.5" or "abc123" are not starts).
fn boundary_before(b: &[u8], start: usize) -> bool {
    if start == 0 {
        return true;
    }
    let c = b[start - 1];
    if c.is_ascii_alphanumeric() || c == b'_' {
        return false;
    }
    // "12.34567890123": a decimal part is not an id.
    if (c == b'.' || c == b',') && start >= 2 && b[start - 2].is_ascii_digit() {
        return false;
    }
    true
}

fn boundary_after(b: &[u8], end: usize) -> bool {
    if end >= b.len() {
        return true;
    }
    let c = b[end];
    if c.is_ascii_alphanumeric() || c == b'_' {
        return false;
    }
    if (c == b'.' || c == b',') && end + 1 < b.len() && b[end + 1].is_ascii_digit() {
        return false;
    }
    true
}

/// Nigerian mobile format: 0 + {7,8,9} + {0,1} + 8 digits (070x, 080x, 081x, 090x, 091x).
fn looks_like_ng_mobile(d: &[u8]) -> bool {
    d.len() == 11 && d[0] == b'0' && matches!(d[1], b'7'..=b'9') && matches!(d[2], b'0' | b'1')
}

fn id_label_before(text: &str, start: usize) -> bool {
    let mut from = start.saturating_sub(24);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let window = text[from..start].to_ascii_lowercase();
    window.contains("bvn") || window.contains("nin")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        redact(s, Options::default()).text
    }

    // ---- cards ----

    #[test]
    fn luhn() {
        assert!(luhn_valid(b"4111111111111111"));
        assert!(luhn_valid(b"5500005555555559"));
        assert!(luhn_valid(b"378282246310005"));
        assert!(!luhn_valid(b"4111111111111112"));
        assert!(!luhn_valid(b""));
    }

    #[test]
    fn masks_cards_plain_spaced_dashed() {
        assert_eq!(r("card 4111111111111111 ok"), "card [CARD] ok");
        assert_eq!(r("card 4111 1111 1111 1111."), "card [CARD].");
        assert_eq!(r("4111-1111-1111-1111"), "[CARD]");
        assert_eq!(r("amex 3782 822463 10005"), "amex [CARD]");
        // Verve, 19 digits
        assert_eq!(r("verve 5061 0000 0000 0000 007"), "verve [CARD]");
        assert_eq!(r("verve 5078950000000000001"), "verve [CARD]");
    }

    #[test]
    fn luhn_false_positives_not_masked() {
        // 16 digits that fail Luhn: order / tracking numbers stay.
        assert_eq!(r("order 4111111111111112"), "order 4111111111111112");
        assert_eq!(r("ref 1234 5678 9012 3456"), "ref 1234 5678 9012 3456");
        // Too short / too long.
        assert_eq!(r("424242424242"), "424242424242");
        assert_eq!(r("41111111111111111111111"), "41111111111111111111111");
        // Embedded in a word.
        assert_eq!(r("id=x4111111111111111"), "id=x4111111111111111");
    }

    #[test]
    fn card_followed_by_other_number_is_still_masked() {
        assert_eq!(r("4111 1111 1111 1111 123"), "[CARD] 123");
    }

    #[test]
    fn two_cards() {
        let red = redact("a 4111111111111111 b 5500-0055-5555-5559", Options::default());
        assert_eq!(red.text, "a [CARD] b [CARD]");
        assert_eq!(red.counts.cards, 2);
    }

    // ---- BVN / NIN ----

    #[test]
    fn masks_bvn_nin() {
        assert_eq!(r("my BVN is 22212345678"), "my BVN is [ID]");
        assert_eq!(r("NIN: 12345678901."), "NIN: [ID].");
        assert_eq!(r("ref 22212345678"), "ref [ID]");
    }

    #[test]
    fn phone_numbers_left_alone_when_distinguishable() {
        assert_eq!(r("call me on 08031234567"), "call me on 08031234567");
        assert_eq!(r("07061234567 or 09121234567"), "07061234567 or 09121234567");
        assert_eq!(r("+2348031234567"), "+2348031234567");
        assert_eq!(r("0803 123 4567"), "0803 123 4567");
        // Labelled as an id: masked even though it looks like a phone number.
        assert_eq!(r("BVN 08031234567"), "BVN [ID]");
    }

    #[test]
    fn non_ids_not_masked() {
        assert_eq!(r("1234567890"), "1234567890"); // 10 digits
        assert_eq!(r("123456789012"), "123456789012"); // 12 digits, not Luhn-valid card length
        assert_eq!(r("v12345678901"), "v12345678901");
        assert_eq!(r("3.14159265358"), "3.14159265358");
        assert_eq!(r("price 2,500,000 naira"), "price 2,500,000 naira");
    }

    // ---- secrets ----

    #[test]
    fn masks_key_value_secrets() {
        assert_eq!(r("password=hunter2"), "password=[REDACTED]");
        assert_eq!(r("PWD: s3cr3t! next"), "PWD: [REDACTED] next");
        assert_eq!(r("api_key = sk-abc123"), "api_key = [REDACTED]");
        assert_eq!(r("token:abc.def"), "token:[REDACTED]");
        assert_eq!(r(r#"password="two words""#), "password=[REDACTED]");
        assert_eq!(r("pin=1234"), "pin=[REDACTED]");
    }

    #[test]
    fn masks_spoken_passwords() {
        assert_eq!(r("my password is hunter2 okay"), "my password is [REDACTED] okay");
        assert_eq!(r("The PIN is 4321"), "The PIN is [REDACTED]");
        assert_eq!(r("passcode was 998877"), "passcode was [REDACTED]");
    }

    #[test]
    fn non_secret_text_untouched() {
        for s in [
            "We passed the test",
            "pinned message",
            "the password field",
            "tokens per minute: 8000",
            "compass=north",
            "I will pass on that",
        ] {
            assert_eq!(r(s), s, "{s}");
        }
    }

    // ---- emails ----

    #[test]
    fn emails_optional() {
        let s = "write to ada.lovelace+x@example.co.uk now";
        assert_eq!(r(s), s);
        let red = redact(s, Options { emails: true });
        assert_eq!(red.text, "write to [EMAIL] now");
        assert_eq!(red.counts.emails, 1);
        assert_eq!(redact("not an email: a@b", Options { emails: true }).text, "not an email: a@b");
    }

    #[test]
    fn prompt_both_halves_and_stable_prefix() {
        let p = Prompt {
            prefix: "About me: pwd=abc".into(),
            dynamic: "card 4111111111111111, BVN 22212345678".into(),
            max_tokens: 60,
            temperature: 0.2,
            stop: vec![],
            images: vec![],
        };
        let (a, c) = redact_prompt(&p, Options::default());
        let (b, _) = redact_prompt(&p, Options::default());
        assert_eq!(a, b);
        assert_eq!(a.prefix, "About me: pwd=[REDACTED]");
        assert_eq!(a.dynamic, "card [CARD], BVN [ID]");
        assert_eq!(c, Counts { cards: 1, ids: 1, secrets: 1, emails: 0 });
        assert_eq!(c.total(), 3);
        assert_eq!(a.max_tokens, 60);
    }

    #[test]
    fn unicode_safe() {
        assert_eq!(r("naïra ₦ BVN—22212345678 ✓"), "naïra ₦ BVN—[ID] ✓");
        assert_eq!(r("ééééééééééééééééééééééé 08031234567"), "ééééééééééééééééééééééé 08031234567");
    }
}
