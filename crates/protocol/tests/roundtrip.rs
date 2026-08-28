use protocol::agent::{AgentCapabilities, AgentToConsole, ConsoleToAgent};
use protocol::channel::{ControlMessage, InputEvent, MouseButton};
use protocol::common::*;
use protocol::config::{AgentConfig, LocalOverrides};
use protocol::ui::{ConsoleToUi, UiToConsole};

#[test]
fn agent_hello_roundtrip() {
    let msg = AgentToConsole::Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        device_id: "dev_1".into(),
        device_secret: "s3cr3t".into(),
        agent_version: "0.1.0".into(),
        hostname: "mac".into(),
        os: Os::Macos,
        arch: Arch::Aarch64,
        mode: DeviceMode::HelpMe,
        capabilities: AgentCapabilities {
            codecs: vec![VideoCodec::H265, VideoCodec::H264],
            displays: vec![DisplayInfo {
                index: 0,
                name: "Built-in".into(),
                x: 0,
                y: 0,
                width: 2880,
                height: 1800,
                scale: 2.0,
                primary: true,
            }],
            input: true,
            clipboard: true,
        },
        logged_in_user: None,
        local_overrides: LocalOverrides::default(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"hello\""));
    assert!(json.contains("\"mode\":\"help_me\""));
    assert!(!json.contains("logged_in_user"));
    let back: AgentToConsole = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn console_session_request_roundtrip() {
    let msg = ConsoleToAgent::SessionRequest {
        session_id: "sess".into(),
        operator: OperatorInfo {
            id: "u1".into(),
            name: "Alice".into(),
        },
        offer: SessionDescription {
            kind: "offer".into(),
            sdp: "v=0...".into(),
        },
        ice_servers: vec![IceServer {
            urls: vec!["turn:turn.example.com:3478".into()],
            username: Some("1700000000:u1".into()),
            credential: Some("hmac".into()),
        }],
        role: SessionRole::Operator,
        shadow_of: None,
        notify_operator: true,
    };
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"session_request\""));
    let back: ConsoleToAgent = serde_json::from_str(&json).unwrap();
    assert_eq!(back, msg);
}

#[test]
fn input_events_use_short_tags() {
    let mm = serde_json::to_string(&InputEvent::MouseMove { x: 10, y: 20 }).unwrap();
    assert_eq!(mm, r#"{"t":"mm","x":10,"y":20}"#);
    let md = serde_json::to_string(&InputEvent::MouseDown {
        button: MouseButton::Left,
    })
    .unwrap();
    assert_eq!(md, r#"{"t":"md","button":"left"}"#);
    let kd: InputEvent = serde_json::from_str(r#"{"t":"kd","code":"KeyA"}"#).unwrap();
    assert_eq!(
        kd,
        InputEvent::KeyDown {
            code: "KeyA".into()
        }
    );
}

#[test]
fn control_and_ui_roundtrip() {
    let c = ControlMessage::SetQuality {
        max_fps: Some(30),
        max_bitrate_kbps: None,
    };
    let json = serde_json::to_string(&c).unwrap();
    assert_eq!(json, r#"{"t":"set_quality","max_fps":30}"#);

    let ui = UiToConsole::SessionOffer {
        device_id: "d".into(),
        offer: SessionDescription {
            kind: "offer".into(),
            sdp: "x".into(),
        },
        shadow_of: None,
    };
    let back: UiToConsole = serde_json::from_str(&serde_json::to_string(&ui).unwrap()).unwrap();
    assert_eq!(back, ui);

    let err = ConsoleToUi::Error {
        session_id: None,
        code: "offline".into(),
        message: "m".into(),
    };
    assert_eq!(
        serde_json::to_string(&err).unwrap(),
        r#"{"type":"error","code":"offline","message":"m"}"#
    );
}

#[test]
fn ice_candidate_matches_browser_field_names() {
    let c = IceCandidate {
        candidate: "candidate:1 1 udp ...".into(),
        sdp_mid: Some("0".into()),
        sdp_mline_index: Some(0),
        username_fragment: None,
    };
    let json = serde_json::to_string(&c).unwrap();
    assert!(json.contains("\"sdpMid\":\"0\""));
    assert!(json.contains("\"sdpMLineIndex\":0"));
}

#[test]
fn default_config_prefers_h265() {
    let cfg = AgentConfig::default();
    assert_eq!(cfg.preferred_codec, VideoCodec::H265);
    assert_eq!(cfg.mode, DeviceMode::Unattended);
}
