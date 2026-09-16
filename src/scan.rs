//! Locating JSON objects inside a raw memory buffer.
//!
//! The whole approach rests on one property of the Warframe client: responses
//! from the DE API arrive as plain JSON text and stay in the heap as plain text
//! after being parsed. So finding a vendor manifest is a string search followed
//! by a brace walk — no struct layouts, no pointer chains, nothing that breaks
//! when the game updates.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// How far to search, and how hard to try, when reconstructing an object.
#[derive(Clone, Copy)]
pub struct ExtractCfg {
    /// Bytes to walk backwards from the needle looking for the opening brace.
    pub max_back: usize,
    /// Cap on candidate opening braces tried per needle hit.
    pub max_candidates: usize,
    /// Cap on the size of a single reconstructed object.
    pub max_object: usize,
}

impl Default for ExtractCfg {
    fn default() -> Self {
        ExtractCfg {
            max_back: 32 << 20,   // 32 MB
            max_candidates: 64,
            max_object: 64 << 20, // 64 MB
        }
    }
}

pub struct Extracted {
    /// Offset of the opening brace within the buffer that was searched.
    pub start: usize,
    pub bytes: Vec<u8>,
    pub value: serde_json::Value,
}

impl Extracted {
    pub fn content_hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.bytes.hash(&mut h);
        h.finish()
    }
}

/// Every occurrence of every needle, sorted by position.
pub fn find_all(data: &[u8], needles: &[Vec<u8>]) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    for (i, n) in needles.iter().enumerate() {
        if n.is_empty() || n.len() > data.len() {
            continue;
        }
        for off in memchr::memmem::find_iter(data, n) {
            hits.push((off, i));
        }
    }
    hits.sort_unstable();
    hits
}

enum ObjMatch {
    /// Balanced object of this byte length.
    Complete(usize),
    /// Ran off the end of the buffer before closing.
    Truncated,
    /// Braces did not balance — not JSON.
    Invalid,
}

/// Walk forward from an opening brace to its match, respecting string literals
/// and escapes so that braces inside strings do not shift the depth.
///
/// This is only a locator: it is fast and linear, and anything it accepts is
/// still handed to serde_json for the real verdict.
fn match_object(data: &[u8], start: usize, limit: usize) -> ObjMatch {
    if start >= data.len() || data[start] != b'{' {
        return ObjMatch::Invalid;
    }
    let end_cap = start.saturating_add(limit).min(data.len());
    let mut depth: usize = 0;
    let mut in_str = false;
    let mut escaped = false;

    for i in start..end_cap {
        let b = data[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => match depth.checked_sub(1) {
                Some(d) => {
                    depth = d;
                    if depth == 0 {
                        return ObjMatch::Complete(i + 1 - start);
                    }
                }
                None => return ObjMatch::Invalid,
            },
            // A NUL inside an unquoted span means the buffer moved on to
            // unrelated heap bytes, so the object cannot close cleanly.
            0 => return ObjMatch::Invalid,
            _ => {}
        }
    }

    if end_cap == data.len() {
        ObjMatch::Truncated
    } else {
        ObjMatch::Invalid
    }
}

/// Opening braces that actually enclose `from`, innermost first.
///
/// Walking backwards and collecting every `{` does not scale: a large response
/// contains one per array element, so a bounded candidate list fills up with
/// nested leaves and never reaches the outer envelope. Instead this tracks brace
/// depth on the way back and keeps only *unmatched* openers, which are by
/// definition the objects that contain the needle. One pass, and the number of
/// candidates is the nesting depth rather than the object count.
///
/// Depth tracking here ignores string literals, so a brace inside a string value
/// would skew it. Warframe payloads are paths and numbers, where that does not
/// occur, and every candidate is still verified by a quote-aware forward match
/// plus a real parse before it is accepted.
fn enclosing_starts(data: &[u8], from: usize, cfg: &ExtractCfg) -> Vec<usize> {
    let floor = from.saturating_sub(cfg.max_back);
    let mut out = Vec::new();
    let mut pending_close: usize = 0;
    let mut i = from;
    loop {
        match data[i] {
            b'}' => pending_close += 1,
            b'{' => {
                if pending_close > 0 {
                    pending_close -= 1;
                } else if i + 1 < data.len() && data[i + 1] == b'"' {
                    out.push(i);
                    if out.len() >= cfg.max_candidates {
                        break;
                    }
                }
            }
            // Heap text is NUL-terminated, so a NUL marks the front edge of the
            // allocation holding this JSON. Nothing before it belongs to it.
            0 => break,
            _ => {}
        }
        if i == floor || i == 0 {
            break;
        }
        i -= 1;
    }
    out
}

/// Reconstruct the outermost valid JSON object that encloses `needle_off`.
///
/// Enclosing braces are tried outermost-first so that a manifest nested inside a
/// response envelope yields the whole envelope. The first candidate that both
/// balances and parses wins; if the outer one is cut off by the end of the buffer
/// the search falls inward to whatever complete object is still recoverable.
pub fn extract_enclosing(
    data: &[u8],
    needle_off: usize,
    cfg: &ExtractCfg,
) -> Option<Extracted> {
    if needle_off >= data.len() {
        return None;
    }

    let starts = enclosing_starts(data, needle_off, cfg);
    for &start in starts.iter().rev() {
        let len = match match_object(data, start, cfg.max_object) {
            ObjMatch::Complete(len) => len,
            ObjMatch::Truncated | ObjMatch::Invalid => continue,
        };
        if start + len <= needle_off {
            continue; // closes before the needle — wrong object
        }
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data[start..start + len]) {
            return Some(Extracted {
                start,
                bytes: data[start..start + len].to_vec(),
                value,
            });
        }
    }
    None
}

/// A one-line description of what a captured object contains.
///
/// Reports the top-level keys plus the length of any array that looks like a
/// vendor stock list, which is enough to tell a real manifest from a fragment
/// without opening the file.
pub fn summarize(v: &serde_json::Value) -> String {
    const STOCK_KEYS: &[&str] = &["ItemManifest", "ItemPrices", "Manifest", "Items", "Inventory"];

    let mut parts: Vec<String> = Vec::new();
    if let Some(obj) = v.as_object() {
        let keys: Vec<&str> = obj.keys().take(8).map(|s| s.as_str()).collect();
        parts.push(format!(
            "keys=[{}{}]",
            keys.join(","),
            if obj.len() > keys.len() { ",…" } else { "" }
        ));
    }

    let mut counts: Vec<String> = Vec::new();
    walk_counts(v, STOCK_KEYS, &mut counts, 0);
    if !counts.is_empty() {
        parts.push(counts.join(" "));
    }

    if let Some(t) = find_string(v, "TypeName", 0) {
        parts.push(format!("type={t}"));
    }
    parts.join("  ")
}

fn walk_counts(v: &serde_json::Value, keys: &[&str], out: &mut Vec<String>, depth: u32) {
    if depth > 6 || out.len() > 6 {
        return;
    }
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if keys.contains(&k.as_str()) {
                if let Some(a) = val.as_array() {
                    out.push(format!("{k}={}", a.len()));
                }
            }
            walk_counts(val, keys, out, depth + 1);
        }
    } else if let Some(arr) = v.as_array() {
        for val in arr.iter().take(4) {
            walk_counts(val, keys, out, depth + 1);
        }
    }
}

/// A short filename-safe tag for a captured object, taken from the manifest
/// `TypeName` when there is one so saved files are identifiable at a glance.
pub fn label(v: &serde_json::Value) -> String {
    let raw = match find_string(v, "TypeName", 0) {
        Some(t) => t,
        None => return "obj".to_string(),
    };
    let tail = raw.rsplit('/').next().unwrap_or("obj");
    let cleaned: String = tail
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "obj".to_string()
    } else {
        cleaned
    }
}

fn find_string(v: &serde_json::Value, key: &str, depth: u32) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match v {
        serde_json::Value::Object(obj) => {
            if let Some(s) = obj.get(key).and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
            obj.values().find_map(|val| find_string(val, key, depth + 1))
        }
        serde_json::Value::Array(arr) => arr
            .iter()
            .take(4)
            .find_map(|val| find_string(val, key, depth + 1)),
        _ => None,
    }
}

/// Printable-ASCII rendering of a byte range, for context snippets.
pub fn render(data: &[u8]) -> String {
    data.iter()
        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '·' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ExtractCfg {
        ExtractCfg::default()
    }

    #[test]
    fn braces_inside_strings_do_not_shift_the_depth() {
        let raw = br#"{"a":"}}}}","b":1}"#;
        match match_object(raw, 0, 1 << 20) {
            ObjMatch::Complete(n) => assert_eq!(n, raw.len()),
            _ => panic!("string contents must not close the object"),
        }
    }

    #[test]
    fn an_escaped_quote_keeps_the_scanner_inside_the_string() {
        let raw = br#"{"a":"x\"}","b":2}"#;
        match match_object(raw, 0, 1 << 20) {
            ObjMatch::Complete(n) => assert_eq!(n, raw.len()),
            _ => panic!("escaped quote must not end the string"),
        }
    }

    #[test]
    fn an_object_running_past_the_buffer_reports_truncated() {
        let raw = br#"{"a":[1,2,3"#;
        assert!(matches!(match_object(raw, 0, 1 << 20), ObjMatch::Truncated));
    }

    #[test]
    fn the_widest_enclosing_object_wins_over_a_nested_one() {
        let mut buf = b"\x00\x01 heap garbage ".to_vec();
        let start = buf.len();
        buf.extend_from_slice(br#"{"VendorInfo":{"ItemManifest":[{"StoreItem":"/Lotus/x"}]}}"#);
        buf.extend_from_slice(b" trailing \x00\xff noise");

        let needle = b"\"StoreItem\"";
        let off = memchr::memmem::find(&buf, needle).unwrap();
        let got = extract_enclosing(&buf, off, &cfg()).expect("object found");

        assert_eq!(got.start, start, "the outer envelope is returned, not the leaf");
        assert!(got.value.get("VendorInfo").is_some());
    }

    #[test]
    fn trailing_heap_noise_is_not_included_in_the_capture() {
        let mut buf = br#"{"ItemManifest":[{"StoreItem":"/Lotus/y"}]}"#.to_vec();
        let json_len = buf.len();
        buf.extend(std::iter::repeat(0xABu8).take(4096));

        let off = memchr::memmem::find(&buf, b"\"StoreItem\"".as_slice()).unwrap();
        let got = extract_enclosing(&buf, off, &cfg()).unwrap();
        assert_eq!(got.bytes.len(), json_len);
    }

    /// The case that matters for real payloads: the needle sits far inside a
    /// large object, behind thousands of sibling `{"` sequences. Collecting
    /// candidate braces nearest-first would exhaust its budget on those siblings
    /// and never reach the envelope.
    #[test]
    fn the_envelope_is_found_behind_many_sibling_objects() {
        let mut json = String::from("{\"Items\":[");
        for i in 0..5000 {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!("{{\"StoreItem\":\"/Lotus/item{i}\"}}"));
        }
        json.push_str("],\"SubscribedToEmails\":0}");

        let mut buf = vec![0u8; 32];
        let start = buf.len();
        buf.extend_from_slice(json.as_bytes());
        buf.push(0);

        let off = memchr::memmem::find(&buf, b"\"SubscribedToEmails\"".as_slice()).unwrap();
        let got = extract_enclosing(&buf, off, &cfg()).expect("envelope found");
        assert_eq!(got.start, start);
        assert_eq!(got.bytes.len(), json.len());
        assert_eq!(got.value["Items"].as_array().unwrap().len(), 5000);
    }

    /// A NUL is the front edge of the heap allocation holding the text, so the
    /// backward walk must not wander into whatever was allocated before it.
    #[test]
    fn the_backward_walk_stops_at_the_allocation_boundary() {
        let mut buf = br#"{"unterminated":[{"a":1}"#.to_vec();
        buf.push(0);
        let inner_start = buf.len();
        buf.extend_from_slice(br#"{"StoreItem":"/Lotus/z"}"#);

        let off = memchr::memmem::find(&buf, b"\"StoreItem\"".as_slice()).unwrap();
        let got = extract_enclosing(&buf, off, &cfg()).expect("object found");
        assert_eq!(got.start, inner_start, "the earlier allocation must not be joined");
    }

    #[test]
    fn a_stray_brace_in_garbage_does_not_produce_a_capture() {
        // `{` followed by a quote, but the contents are not JSON.
        let mut buf = b"{\"\x01\x02\x03 not json at all ".to_vec();
        buf.extend_from_slice(b"\"StoreItem\" \x04\x05");
        let off = memchr::memmem::find(&buf, b"\"StoreItem\"".as_slice()).unwrap();
        assert!(extract_enclosing(&buf, off, &cfg()).is_none());
    }

    #[test]
    fn summarize_reports_the_stock_count_and_manifest_type() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"VendorInfo":{"TypeName":"/Lotus/Types/Game/VendorManifests/Zariman/Z",
                 "ItemManifest":[{"a":1},{"b":2},{"c":3}]}}"#,
        )
        .unwrap();
        let s = summarize(&v);
        assert!(s.contains("ItemManifest=3"), "got: {s}");
        assert!(s.contains("VendorManifests/Zariman/Z"), "got: {s}");
    }

    #[test]
    fn identical_captures_hash_the_same_and_different_ones_do_not() {
        let a = Extracted {
            start: 0,
            bytes: b"{\"a\":1}".to_vec(),
            value: serde_json::json!({"a": 1}),
        };
        let b = Extracted {
            start: 999,
            bytes: b"{\"a\":1}".to_vec(),
            value: serde_json::json!({"a": 1}),
        };
        let c = Extracted {
            start: 0,
            bytes: b"{\"a\":2}".to_vec(),
            value: serde_json::json!({"a": 2}),
        };
        assert_eq!(a.content_hash(), b.content_hash());
        assert_ne!(a.content_hash(), c.content_hash());
    }
}
