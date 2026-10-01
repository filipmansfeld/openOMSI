//! Bounded protocol shared by the frame thread and local socket workers.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

pub(crate) const MAX_JSON: usize = 2 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub(crate) protocol: u32,
    pub(crate) token: String,
    pub(crate) request_id: u64,
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) vehicle_id: u64,
    #[serde(default)]
    pub(crate) generation: u64,
    #[serde(default)]
    pub(crate) values: BTreeMap<String, f32>,
    #[serde(default)]
    pub(crate) strings: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) path: String,
    /// Named operations put their arguments here; the existing set_variables
    /// and invalidate_texture clients use the top-level fields instead.
    #[serde(default)]
    pub(crate) args: Option<Value>,
}

pub(crate) fn validate_values(request: &Request) -> Result<(), String> {
    if request.values.len() + request.strings.len() > 512 {
        return Err("too many variables in one request".into());
    }
    if request
        .values
        .iter()
        .any(|(n, v)| n.is_empty() || n.len() > 256 || !v.is_finite())
        || request
            .strings
            .iter()
            .any(|(n, v)| n.is_empty() || n.len() > 256 || v.len() > 65536)
    {
        return Err("invalid variable name or value".into());
    }
    for names in [
        request.values.keys().collect::<Vec<_>>(),
        request.strings.keys().collect::<Vec<_>>(),
    ] {
        let mut seen = std::collections::BTreeSet::new();
        if names
            .into_iter()
            .any(|name| !seen.insert(name.to_ascii_lowercase()))
        {
            return Err(
                "variable names differing only by ASCII case are ambiguous in one transaction"
                    .into(),
            );
        }
    }
    Ok(())
}

pub(crate) fn authenticated(expected: &str, actual: &str) -> bool {
    expected.len() == actual.len()
        && expected
            .bytes()
            .zip(actual.bytes())
            .fold(0u8, |d, (a, b)| d | (a ^ b))
            == 0
}

pub(crate) fn read_frame(reader: &mut impl Read) -> io::Result<Request> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let n = u32::from_le_bytes(length) as usize;
    if n == 0 || n > MAX_JSON {
        return Err(io::Error::other("invalid JSON frame length"));
    }
    let mut bytes = vec![0; n];
    reader.read_exact(&mut bytes)?;
    let request = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    // Retain the protocol's trailing binary-length word for existing clients,
    // but reject every nonzero value before allocating or reading any payload.
    reader.read_exact(&mut length)?;
    if u32::from_le_bytes(length) != 0 {
        return Err(io::Error::other("this API does not accept binary payloads"));
    }
    Ok(request)
}

pub(crate) fn write_response(stream: &mut impl Write, response: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_JSON {
        // Serialization happens after execution: a failed reply cannot promise
        // that a trigger or variable write was rolled back.
        bytes = serde_json::to_vec(&serde_json::json!({"protocol":1,
            "request_id":response.get("request_id"),"ok":false,
            "status":"completed_reply_too_large",
            "error":"response exceeds 2 MiB; a requested mutation may already have applied"}))?;
    }
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.write_all(&0u32.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(value: Value, binary_len: u32) -> Vec<u8> {
        let bytes = serde_json::to_vec(&value).unwrap();
        let mut frame = (bytes.len() as u32).to_le_bytes().to_vec();
        frame.extend(bytes);
        frame.extend(binary_len.to_le_bytes());
        frame
    }

    fn request() -> Value {
        json!({"protocol":1,"token":"test","request_id":7,"op":"snapshot"})
    }

    #[test]
    fn frame_limits_and_unknown_fields_are_rejected() {
        for size in [0, MAX_JSON as u32 + 1, u32::MAX] {
            assert!(read_frame(&mut io::Cursor::new(size.to_le_bytes())).is_err());
        }
        for size in [1, u32::MAX] {
            let mut reader = io::Cursor::new(frame(request(), size));
            let error = read_frame(&mut reader).unwrap_err();
            assert!(error.to_string().contains("binary payloads"));
        }
        let mut value = request();
        value["clock"] = json!({"service_seconds":5});
        assert!(read_frame(&mut io::Cursor::new(frame(value, 0))).is_err());
        let valid = read_frame(&mut io::Cursor::new(frame(request(), 0))).unwrap();
        assert_eq!(valid.request_id, 7);
    }

    #[test]
    fn authentication_and_variable_limits() {
        assert!(authenticated("abc", "abc"));
        assert!(!authenticated("abc", "abd"));
        assert!(!authenticated("abc", "abcx"));
        let mut request: Request = serde_json::from_value(request()).unwrap();
        request.values.insert("speed".into(), 1.0);
        assert!(validate_values(&request).is_ok());
        request.values.insert("bad".into(), f32::INFINITY);
        assert!(validate_values(&request).is_err());
        request.values.remove("bad");
        request.values.insert("Speed".into(), 2.0);
        assert!(validate_values(&request).is_err());
        request.values.clear();
        request.strings.insert("display".into(), "x".repeat(65537));
        assert!(validate_values(&request).is_err());
    }

    #[test]
    fn oversized_reply_keeps_a_bounded_correlated_error_frame() {
        let mut output = Vec::new();
        write_response(
            &mut output,
            &json!({"request_id":42,"result":"x".repeat(MAX_JSON)}),
        )
        .unwrap();
        let n = u32::from_le_bytes(output[..4].try_into().unwrap()) as usize;
        assert!(n < 1024);
        let response: Value = serde_json::from_slice(&output[4..4 + n]).unwrap();
        assert_eq!(response["request_id"], 42);
        assert_eq!(response["status"], "completed_reply_too_large");
        assert_eq!(&output[4 + n..], &[0, 0, 0, 0]);
    }
}
