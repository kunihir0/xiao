//! Panic-free Rust+ protocol client and camera payload decoder.
//!
//! Wire compatibility is pinned to `kunihir0/rustplus` master at
//! `5b9a0803ac404a00b7f58e298c91b1130504d5c7`. The generated protobuf module
//! is private; callers receive app-neutral request, response, and event values.

mod client;
mod proto {
    #![allow(clippy::all, clippy::pedantic, non_snake_case, non_camel_case_types)]
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/rustplus.rs"));
}
pub mod camera;
mod error;
mod types;

pub use client::{ClientConfig, EventReceiver, RustPlusClient};
pub use error::{Error, Result};
pub use types::{
    CameraRays, Event, EventReceiveError, Marker, MarkerKind, RatePolicy, Readiness,
    ReconnectPolicy, Request, Response, ResponsePayload,
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use prost::Message;

    fn fixture(name: &str) -> Vec<u8> {
        include_str!("../tests/fixtures/legacy-wire.txt")
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(" = ")?;
                (key == name).then(|| {
                    value
                        .split_whitespace()
                        .map(|byte| u8::from_str_radix(byte, 16).expect("hex fixture"))
                        .collect()
                })
            })
            .expect("fixture entry")
    }

    #[test]
    fn request_fixture_matches_legacy_wire_bytes() {
        let request = crate::client::to_proto_request(1, 42, 7, Request::Info);
        let mut encoded = Vec::new();
        request.encode(&mut encoded).expect("fixture encoding");
        assert_eq!(encoded, fixture("request_info"));
    }

    #[test]
    fn response_fixture_decodes_known_success() {
        let message = proto::AppMessage::decode(fixture("response_success").as_slice())
            .expect("fixture decode");
        let response = message.response.expect("response");
        assert_eq!(response.seq, 1);
        assert!(response.success.is_some());
    }

    #[test]
    fn malformed_fixtures_return_typed_errors() {
        for name in ["malformed_truncated", "malformed_varint", "malformed_length"] {
            assert!(proto::AppMessage::decode(fixture(name).as_slice()).is_err(), "{name}");
        }
    }

    #[test]
    fn camera_event_fixture_preserves_payload() {
        let message = proto::AppMessage::decode(fixture("event_camera").as_slice())
            .expect("event fixture decode");
        let event = crate::client::convert_event(message.broadcast).expect("camera event");
        assert_eq!(
            event,
            Event::CameraRays(CameraRays {
                vertical_fov: 1.0,
                sample_offset: 0,
                ray_data: vec![255, 0, 0, 0],
                distance: 1.0,
            })
        );
    }

    #[test]
    fn camera_boundaries_are_rejected() {
        assert!(camera::FrameBuilder::new(0, 1).is_err());
        assert!(camera::FrameBuilder::new(4097, 1).is_err());
        let mut frame = camera::FrameBuilder::new(1, 1).expect("valid dimensions");
        let rays = CameraRays {
            vertical_fov: 1.0,
            sample_offset: 0,
            ray_data: vec![255],
            distance: 1.0,
        };
        assert!(frame.ingest(&rays).is_err());
    }

    #[test]
    fn dropped_request_guard_removes_pending() {
        use std::{
            collections::HashMap,
            sync::{Arc, Mutex},
        };
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender, _receiver) = tokio::sync::oneshot::channel();
        pending.lock().expect("pending lock").insert(7, sender);
        let guard = crate::client::PendingGuard {
            pending: Arc::clone(&pending),
            sequence: 7,
            armed: true,
        };
        drop(guard);
        assert!(pending.lock().expect("pending lock").is_empty());
    }

    #[test]
    fn reconnect_policy_is_bounded() {
        let policy = ReconnectPolicy::default();
        assert!(policy.delay(100, 100) <= policy.max_delay);
    }
}
