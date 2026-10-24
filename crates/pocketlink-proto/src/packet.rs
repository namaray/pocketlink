// SPDX-License-Identifier: GPL-3.0-or-later

//! Network packets: one JSON object per line.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const IDENTITY: &str = "kdeconnect.identity";
pub const PAIR: &str = "kdeconnect.pair";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Packet {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub body: Map<String, Value>,
    #[serde(
        rename = "payloadSize",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_opt_i64"
    )]
    pub payload_size: Option<i64>,
    #[serde(
        rename = "payloadTransferInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub payload_transfer_info: Option<Map<String, Value>>,
}

impl Packet {
    /// `body` must be a JSON object (or null for an empty body).
    pub fn new(kind: impl Into<String>, body: Value) -> Self {
        let body = match body {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        Self {
            id: 0,
            kind: kind.into(),
            body,
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    /// Serialized form, newline terminated, with a fresh `id`.
    pub fn to_line(&self) -> Vec<u8> {
        let mut packet = self.clone();
        packet.id = now_millis();
        let mut line = serde_json::to_vec(&packet).expect("packets always serialize");
        line.push(b'\n');
        line
    }

    pub fn from_line(line: &[u8]) -> Option<Self> {
        let packet: Packet = serde_json::from_slice(line).ok()?;
        (!packet.kind.is_empty()).then_some(packet)
    }

    pub fn is(&self, kind: &str) -> bool {
        self.kind == kind
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.body.insert(key.to_owned(), value.into());
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.body.get(key)?.as_str()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.body.get(key)? {
            Value::Bool(b) => Some(*b),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// Accepts numbers and numeric strings; implementations disagree on which they send.
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        as_i64(self.body.get(key)?)
    }

    pub fn get_str_list(&self, key: &str) -> Vec<String> {
        match self.body.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The TCP port the sender is offering this packet's payload on.
    pub fn payload_port(&self) -> Option<u16> {
        let port = as_i64(self.payload_transfer_info.as_ref()?.get("port")?)?;
        u16::try_from(port).ok()
    }
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn lenient_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(lenient_opt_i64(d)?.unwrap_or_default())
}

fn lenient_opt_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let value = Option::<Value>::deserialize(d)?;
    Ok(value.as_ref().and_then(as_i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips() {
        let mut packet = Packet::new("kdeconnect.ping", json!({ "message": "hi" }));
        packet.payload_size = Some(10);
        let line = packet.to_line();
        assert_eq!(line.last(), Some(&b'\n'));
        let parsed = Packet::from_line(&line).unwrap();
        assert_eq!(parsed.kind, "kdeconnect.ping");
        assert_eq!(parsed.get_str("message"), Some("hi"));
        assert_eq!(parsed.payload_size, Some(10));
        assert!(parsed.id > 0);
    }

    #[test]
    fn tolerates_string_numbers() {
        let line = br#"{"id":"123","type":"x","body":{"n":"8"},"payloadTransferInfo":{"port":"1739"}}"#;
        let packet = Packet::from_line(line).unwrap();
        assert_eq!(packet.id, 123);
        assert_eq!(packet.get_i64("n"), Some(8));
        assert_eq!(packet.payload_port(), Some(1739));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Packet::from_line(b"not json").is_none());
        assert!(Packet::from_line(br#"{"type":""}"#).is_none());
    }
}
