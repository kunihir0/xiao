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
    CameraEntity, CameraEntityKind, CameraRays, ChatMessage, EntityInfo, EntityItem, EntityPayload,
    Event, EventReceiveError, MapNote, Marker, MarkerKind, MonitorPolicy, RatePolicy, Readiness,
    ReconnectPolicy, Request, Response, ResponsePayload, ServerInfo, ServerTime, TeamInfo,
    TeamMember,
};

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use prost::Message;

    fn fixture(name: &str) -> Vec<u8> {
        let text = include_str!("../tests/fixtures/legacy-wire.txt");
        let value = text
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} = ")))
            .expect("fixture key");
        value
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("fixture byte"))
            .collect()
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
    fn monitor_fixture_preserves_marker_payload() {
        let message = proto::AppMessage::decode(fixture("monitor_markers").as_slice())
            .expect("monitor fixture decode");
        let response =
            crate::client::convert_response(&message.response.expect("monitor response"))
                .expect("marker conversion");
        assert!(
            matches!(response.payload, ResponsePayload::MapMarkers(markers) if markers.len() == 1 && markers[0].id == 42 && markers[0].name.as_deref() == Some("Joe"))
        );
    }
    #[test]
    fn event_fixture_preserves_camera_payload() {
        let message = proto::AppMessage::decode(fixture("event_camera").as_slice())
            .expect("event fixture decode");
        assert!(matches!(
            crate::client::convert_event(message.broadcast),
            Some(Event::CameraRays(_))
        ));
    }
    #[test]
    fn malformed_wire_is_typed_error() {
        assert!(proto::AppMessage::decode(fixture("malformed_truncated").as_slice()).is_err());
    }
    #[test]
    fn malformed_varints_and_lengths_are_rejected() {
        let malformed = [
            vec![0x08, 0x80],
            vec![0x0a, 0xff, 0xff, 0xff, 0x7f],
            vec![0x0a, 0x02, 0x08],
        ];
        for bytes in malformed {
            assert!(proto::AppMessage::decode(bytes.as_slice()).is_err());
        }
    }
    #[test]
    fn camera_fixture_preserves_legacy_sample_position() {
        let mut frame = camera::FrameBuilder::new(2, 2).expect("valid dimensions");
        let rays = CameraRays {
            vertical_fov: 1.0,
            sample_offset: 0,
            ray_data: fixture("camera_2x2_single_ray"),
            distance: 1.0,
            entities: Vec::new(),
        };
        assert_eq!(frame.ingest(&rays).expect("camera sample"), 1);
        assert_eq!(frame.image().rgba[7], 255);
        assert_eq!(
            frame
                .image()
                .rgba
                .iter()
                .filter(|&&channel| channel == 255)
                .count(),
            1
        );
    }
    #[test]
    fn camera_boundaries_and_excess_tokens_are_rejected() {
        assert!(camera::FrameBuilder::new(0, 1).is_err());
        assert!(camera::FrameBuilder::new(4097, 1).is_err());
        let mut frame = camera::FrameBuilder::new(1, 1).expect("valid dimensions");
        let rays = CameraRays {
            vertical_fov: 1.0,
            sample_offset: 0,
            ray_data: vec![255],
            distance: 1.0,
            entities: Vec::new(),
        };
        assert!(frame.ingest(&rays).is_err());
    }
    #[test]
    fn invalid_rate_policy_is_typed() {
        assert!(
            RatePolicy {
                capacity: f64::NAN,
                refill_per_second: 1.0
            }
            .validate()
            .is_err()
        );
        assert!(
            RatePolicy {
                capacity: 1.0,
                refill_per_second: 0.0
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn reconnect_jitter_is_bounded_and_varies() {
        let policy = ReconnectPolicy::default();
        let a = policy.delay_with_jitter(2, 20, 0);
        let b = policy.delay_with_jitter(2, 20, 255);
        assert!(a <= policy.max_delay);
        assert!(b <= policy.max_delay);
        assert_ne!(a, b);
    }
    #[test]
    fn client_debug_redacts_token() {
        let debug = format!(
            "{:?}",
            ClientConfig::new("wss://example.invalid", 42, 987654)
        );
        assert!(!debug.contains("987654"));
        assert!(debug.contains("REDACTED"));
    }
}
