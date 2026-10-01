//! Shared bounded wire format used by the game and the standalone protocol checks.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

pub(crate) const MAX_JSON: usize = 2 * 1024 * 1024;
pub(crate) const MAX_BINARY: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
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
    pub(crate) tile_index: usize,
    #[serde(default)]
    pub(crate) object_index: usize,
    #[serde(default)]
    pub(crate) section: usize,
    #[serde(default)]
    pub(crate) index: usize,
    #[serde(default)]
    pub(crate) width: u32,
    #[serde(default)]
    pub(crate) height: u32,
    #[serde(default)]
    pub(crate) format: String,
    #[serde(default)]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) clock: Option<ClockUpdate>,
    /// Named native API operations put their arguments here. Legacy operations keep
    /// their original top-level fields for existing bridge clients.
    #[serde(default)]
    pub(crate) args: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClockUpdate {
    pub(crate) service_seconds: Option<f64>,
    pub(crate) year: Option<i32>,
    pub(crate) day_of_year: Option<i32>,
}

impl ClockUpdate {
    /// Validate the entire update before changing the running simulation.
    pub(crate) fn validate(
        &self,
        current_year: i32,
        current_day: i32,
    ) -> Result<(i32, i32), String> {
        if self.service_seconds.is_none() && self.year.is_none() && self.day_of_year.is_none() {
            return Err("clock update is empty".into());
        }
        if self
            .service_seconds
            .is_some_and(|s| !s.is_finite() || !(0.0..86400.0).contains(&s))
        {
            return Err("clock time must be finite seconds since midnight below 86400".into());
        }
        let year = self.year.unwrap_or(current_year);
        let day = self.day_of_year.unwrap_or(current_day);
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        if !(1..=9999).contains(&year) || !(1..=if leap { 366 } else { 365 }).contains(&day) {
            return Err("clock date is outside the valid year/day-of-year range".into());
        }
        Ok((year, day))
    }
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

pub(crate) fn decode_bgra(request: &Request, mut binary: Vec<u8>) -> Result<Vec<u8>, String> {
    if request.format != "bgra8"
        || request.width == 0
        || request.height == 0
        || request.width > 2048
        || request.height > 2048
        || request.index > 63
        || binary.len() != request.width as usize * request.height as usize * 4
    {
        return Err(
            "invalid script texture dimensions, format, section, index or byte count".into(),
        );
    }
    for pixel in binary.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(binary)
}

pub(crate) fn authenticated(expected: &str, actual: &str) -> bool {
    expected.len() == actual.len()
        && expected
            .bytes()
            .zip(actual.bytes())
            .fold(0u8, |d, (a, b)| d | (a ^ b))
            == 0
}

pub(crate) fn read_frame(reader: &mut impl Read) -> io::Result<(Request, Vec<u8>)> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let n = u32::from_le_bytes(length) as usize;
    if n == 0 || n > MAX_JSON {
        return Err(io::Error::other("invalid JSON frame length"));
    }
    let mut json_bytes = vec![0; n];
    reader.read_exact(&mut json_bytes)?;
    let request = serde_json::from_slice::<Request>(&json_bytes).map_err(io::Error::other)?;
    reader.read_exact(&mut length)?;
    let n = u32::from_le_bytes(length) as usize;
    if n > MAX_BINARY
        || (!matches!(request.op.as_str(), "script_texture" | "texture.upload") && n != 0)
    {
        return Err(io::Error::other("invalid binary frame length"));
    }
    let mut binary = vec![0; n];
    reader.read_exact(&mut binary)?;
    Ok((request, binary))
}

pub(crate) fn write_response(stream: &mut impl Write, response: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_JSON {
        // Preserve the connection and tell callers why the response is unavailable.
        // A mutation may already have applied, so never describe this as rejection.
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

    fn request(extra: Value) -> Request {
        let mut value = json!({"protocol":1,"token":"test","request_id":7,"op":"script_texture"});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn binary_frame_limits_are_checked_before_allocating() {
        assert!(read_frame(&mut io::Cursor::new((MAX_JSON as u32 + 1).to_le_bytes())).is_err());
        let json = br#"{"protocol":1,"token":"test","request_id":7,"op":"script_texture"}"#;
        let mut frame = (json.len() as u32).to_le_bytes().to_vec();
        frame.extend(json);
        frame.extend((MAX_BINARY as u32 + 1).to_le_bytes());
        assert!(read_frame(&mut io::Cursor::new(frame)).is_err());
    }

    #[test]
    fn pixels_preserve_alpha_and_reject_mismatched_dimensions() {
        let req = request(json!({"width":1,"height":1,"format":"bgra8"}));
        assert_eq!(
            decode_bgra(&req, vec![3, 2, 1, 128]).unwrap(),
            vec![1, 2, 3, 128]
        );
        assert!(decode_bgra(&req, vec![3, 2, 1]).is_err());
        let huge = request(json!({"width":2049,"height":1,"format":"bgra8"}));
        assert!(decode_bgra(&huge, vec![]).is_err());
    }

    #[test]
    fn wrong_tokens_and_invalid_variable_batches_are_rejected() {
        assert!(authenticated("abc", "abc"));
        assert!(!authenticated("abc", "abd"));
        assert!(!authenticated("abc", "abcx"));
        let mut req = request(json!({"values":{"speed":1.0}}));
        req.values.insert("bad".into(), f32::INFINITY);
        assert!(validate_values(&req).is_err());
        let req = request(json!({"values":{"speed":1.0,"Speed":2.0}}));
        assert!(validate_values(&req).is_err());
    }

    #[test]
    fn clock_updates_reject_invalid_partial_dates_without_clamping() {
        let update = ClockUpdate {
            year: Some(2023),
            ..Default::default()
        };
        assert!(update.validate(2024, 366).is_err());
        let update = ClockUpdate {
            service_seconds: Some(86399.125),
            day_of_year: Some(366),
            ..Default::default()
        };
        assert_eq!(update.validate(2024, 1).unwrap(), (2024, 366));
        assert!(update.validate(2023, 1).is_err());
        assert!(ClockUpdate {
            service_seconds: Some(f64::NAN),
            ..Default::default()
        }
        .validate(2024, 1)
        .is_err());
        assert!(ClockUpdate::default().validate(2024, 1).is_err());
        assert!(serde_json::from_value::<ClockUpdate>(json!({"year":"2024"})).is_err());
        assert!(serde_json::from_value::<ClockUpdate>(json!({"unknown":1})).is_err());
    }

    #[test]
    fn oversized_reply_keeps_a_bounded_correlated_error_frame() {
        let mut bytes = Vec::new();
        write_response(
            &mut bytes,
            &json!({"protocol":1,"request_id":42,"ok":true,"result":"x".repeat(MAX_JSON)}),
        )
        .unwrap();
        let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert!(count < 1024);
        let reply: Value = serde_json::from_slice(&bytes[4..4 + count]).unwrap();
        assert_eq!(reply["request_id"], 42);
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["status"], "completed_reply_too_large");
        assert_eq!(&bytes[4 + count..], &[0, 0, 0, 0]);
    }
}
