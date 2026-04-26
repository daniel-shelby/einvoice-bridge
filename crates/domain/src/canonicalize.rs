//! RFC 8785 JSON Canonicalisation Scheme (JCS).
//!
//! LHDN MyInvois requires the JSON document to be canonicalised before
//! hashing/signing. RFC 8785 specifies:
//!   - Object members are sorted lexicographically by their UTF-16 code-unit
//!     sequence.
//!   - No insignificant whitespace.
//!   - Numbers are serialised with the ECMAScript `ToString(Number)`
//!     algorithm (the genuinely tricky part — it does not match Rust's
//!     default `f64::to_string`).
//!
//! We delegate to `serde_jcs`, which uses `ryu-js` for the number
//! formatter so the output is bit-for-bit RFC 8785.

use crate::DomainError;
use serde_json::Value;

/// Canonicalise a `serde_json::Value` to its RFC 8785 string form.
pub fn canonicalize_json(value: &Value) -> Result<String, DomainError> {
    serde_jcs::to_string(value).map_err(|e| DomainError::Canonicalize(e.to_string()))
}

/// Convenience: canonicalise and return the UTF-8 bytes (what you actually
/// hash and sign).
pub fn canonicalize_json_bytes(value: &Value) -> Result<Vec<u8>, DomainError> {
    canonicalize_json(value).map(String::into_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_object_keys_lexicographically() {
        let input = json!({"b": 1, "a": 2, "c": 3});
        assert_eq!(
            canonicalize_json(&input).unwrap(),
            r#"{"a":2,"b":1,"c":3}"#
        );
    }

    #[test]
    fn strips_insignificant_whitespace() {
        let input: Value = serde_json::from_str(r#"{ "a" : 1 , "b" : [ 2 , 3 ] }"#).unwrap();
        assert_eq!(
            canonicalize_json(&input).unwrap(),
            r#"{"a":1,"b":[2,3]}"#
        );
    }

    #[test]
    fn arrays_preserve_their_order() {
        let input = json!([3, 1, 2]);
        assert_eq!(canonicalize_json(&input).unwrap(), "[3,1,2]");
    }

    #[test]
    fn nested_objects_are_sorted_recursively() {
        let input = json!({
            "outer": {"z": 1, "a": 2},
            "first": "hi"
        });
        assert_eq!(
            canonicalize_json(&input).unwrap(),
            r#"{"first":"hi","outer":{"a":2,"z":1}}"#
        );
    }

    #[test]
    fn numbers_use_ecmascript_tostring() {
        // Trailing zeros stripped, exponential picked when shorter, integers stay integers.
        let input = json!({ "n": [4.50, 2e-3, 1e30, 100, 0] });
        assert_eq!(
            canonicalize_json(&input).unwrap(),
            r#"{"n":[4.5,0.002,1e+30,100,0]}"#
        );
    }

    #[test]
    fn named_escapes_quote_and_backslash() {
        // \n and \t use named escapes; quote and backslash are escaped.
        let input = json!("a\nb\tc\"d\\e");
        assert_eq!(
            canonicalize_json(&input).unwrap(),
            r#""a\nb\tc\"d\\e""#
        );
    }

    #[test]
    fn unicode_above_ascii_is_emitted_verbatim() {
        // RFC 8785 emits non-control non-ASCII characters as raw UTF-8,
        // not \uXXXX escapes.
        let input = json!("€");
        assert_eq!(canonicalize_json(&input).unwrap(), "\"€\"");
    }

    #[test]
    fn round_trip_canonical_is_idempotent() {
        let input = json!({"z": 1, "a": [3, 2, 1], "m": {"y": true, "x": null}});
        let once = canonicalize_json(&input).unwrap();
        let parsed: Value = serde_json::from_str(&once).unwrap();
        let twice = canonicalize_json(&parsed).unwrap();
        assert_eq!(once, twice);
    }
}
