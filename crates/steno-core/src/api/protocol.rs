//! NDJSON IPC protocol types and framing helpers.
//!
//! One JSON object per line. Requests are flattened so the wire form is
//! `{"id":1,"op":"ping"}` (not a nested `op` object).


use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Client → server request. Optional `token` is evaluated against configured API token auth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Unique request identifier for matching responses.
    pub id: u64,
    /// Optional authentication token for request authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Operation discriminant and payload.
    #[serde(flatten)]
    pub op: Op,
}

/// Operation discriminant + payload. Tagged by the `op` string on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "status")]
    Status,
    #[serde(rename = "transcribe")]
    Transcribe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wav_path: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pcm_f32_b64: Option<String>,
    },
    #[serde(rename = "utterance.start")]
    UtteranceStart,
    #[serde(rename = "utterance.audio")]
    UtteranceAudio { pcm_f32_b64: String },
    #[serde(rename = "utterance.stop")]
    UtteranceStop,
    #[serde(rename = "utterance.cancel")]
    UtteranceCancel,
    #[serde(rename = "shutdown")]
    Shutdown,
}

/// Server → client reply to a request `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Request identifier matching the client request.
    pub id: u64,
    /// Indicates whether the operation succeeded.
    pub ok: bool,
    /// Operation result payload on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error message on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Actionable hint or remediation advice on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Response {
    pub fn ok(id: u64, result: Option<Value>) -> Self {
        Self {
            id,
            ok: true,
            result,
            error: None,
            hint: None,
        }
    }

    pub fn err(id: u64, error: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.into()),
            hint,
        }
    }
}

/// Unsolicited server → client notifications (no `id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    #[serde(rename = "stage")]
    Stage { stage: String },
    #[serde(rename = "transcript")]
    Transcript {
        text: String,
        #[serde(rename = "final")]
        final_: bool,
    },
    /// Final text for a completed `utterance.*` session (fan-out reserved;
    /// `utterance.stop` also returns the same text in the `Response` result).
    #[serde(rename = "utterance.done")]
    UtteranceDone { text: String },
}

/// Serialize `value` as a single NDJSON line (trailing `\n`).
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// Deserialize one NDJSON line (trailing whitespace / `\r` / `\n` stripped).
pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\r', '\n']).trim())
}

/// Best-effort extract of `id` from a malformed request line so we can still
/// reply with an error instead of dropping the frame.
pub fn peek_request_id(line: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(line.trim_end_matches(['\r', '\n']).trim()).ok()?;
    v.get("id")?.as_u64()
}

#[cfg(test)]
mod tests {
    //! WHY: Protocol framing, request/response serialization, op discrimination, and error payload
    //! formatting must stay wire-compatible across IPC API updates.
    use super::*;
    use serde_json::json;

    fn round_trip_op(op: Op, expected_op: &str) {
        let req = Request {
            id: 7,
            token: None,
            op,
        };
        let line = encode_line(&req).expect("encode");
        assert!(line.ends_with('\n'));
        let back: Request = decode_line(&line).expect("decode");
        assert_eq!(back, req);
        let v: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["op"], expected_op);
    }

    #[test]
    fn op_ping_round_trip() {
        round_trip_op(Op::Ping, "ping");
    }

    #[test]
    fn op_status_round_trip() {
        round_trip_op(Op::Status, "status");
    }

    #[test]
    fn op_transcribe_wav_round_trip() {
        round_trip_op(
            Op::Transcribe {
                wav_path: Some(PathBuf::from("/tmp/a.wav")),
                pcm_f32_b64: None,
            },
            "transcribe",
        );
    }

    #[test]
    fn op_transcribe_pcm_round_trip() {
        round_trip_op(
            Op::Transcribe {
                wav_path: None,
                pcm_f32_b64: Some("AAAA".into()),
            },
            "transcribe",
        );
    }

    #[test]
    fn op_utterance_start_round_trip() {
        round_trip_op(Op::UtteranceStart, "utterance.start");
    }

    #[test]
    fn op_utterance_audio_round_trip() {
        round_trip_op(
            Op::UtteranceAudio {
                pcm_f32_b64: "AQID".into(),
            },
            "utterance.audio",
        );
    }

    #[test]
    fn op_utterance_stop_round_trip() {
        round_trip_op(Op::UtteranceStop, "utterance.stop");
    }

    #[test]
    fn op_utterance_cancel_round_trip() {
        round_trip_op(Op::UtteranceCancel, "utterance.cancel");
    }

    #[test]
    fn op_shutdown_round_trip() {
        round_trip_op(Op::Shutdown, "shutdown");
    }

    #[test]
    fn request_with_token_round_trip() {
        let req = Request {
            id: 3,
            token: Some("secret".into()),
            op: Op::Ping,
        };
        let back: Request = decode_line(&encode_line(&req).unwrap()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn response_ok_and_err_round_trip() {
        let ok = Response::ok(1, Some(json!({"text": "hello"})));
        let err = Response::err(
            2,
            "boom",
            Some("fix the path and retry".into()),
        );
        assert_eq!(
            decode_line::<Response>(&encode_line(&ok).unwrap()).unwrap(),
            ok
        );
        assert_eq!(
            decode_line::<Response>(&encode_line(&err).unwrap()).unwrap(),
            err
        );
        let ok_v: Value = serde_json::from_str(encode_line(&ok).unwrap().trim_end()).unwrap();
        assert_eq!(ok_v, json!({"id":1,"ok":true,"result":{"text":"hello"}}));
    }

    #[test]
    fn event_stage_and_transcript_round_trip() {
        let stage = Event::Stage {
            stage: "listening".into(),
        };
        let transcript = Event::Transcript {
            text: "hello".into(),
            final_: true,
        };
        assert_eq!(
            decode_line::<Event>(&encode_line(&stage).unwrap()).unwrap(),
            stage
        );
        assert_eq!(
            decode_line::<Event>(&encode_line(&transcript).unwrap()).unwrap(),
            transcript
        );
        let t_v: Value =
            serde_json::from_str(encode_line(&transcript).unwrap().trim_end()).unwrap();
        assert_eq!(
            t_v,
            json!({"event":"transcript","text":"hello","final":true})
        );
    }

    #[test]
    fn event_utterance_done_round_trip() {
        let done = Event::UtteranceDone {
            text: "hello world".into(),
        };
        assert_eq!(
            decode_line::<Event>(&encode_line(&done).unwrap()).unwrap(),
            done
        );
        let v: Value = serde_json::from_str(encode_line(&done).unwrap().trim_end()).unwrap();
        assert_eq!(
            v,
            json!({"event":"utterance.done","text":"hello world"})
        );
    }

    #[test]
    fn peek_request_id_from_partial_object() {
        assert_eq!(peek_request_id(r#"{"id":42,"op":"nope"}"#), Some(42));
        assert_eq!(peek_request_id("not-json"), None);
        assert_eq!(peek_request_id(r#"{"op":"ping"}"#), None);
    }
}
