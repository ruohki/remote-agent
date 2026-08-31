//! What the agent does before it can talk to the console: decide the start state (enrolled,
//! baked token, or nothing) and, in the application window, run the Connect screen until an
//! enrollment succeeds.

use crate::app::{ConnectRequest, ConnectUi};
use crate::config::{LocalConfig, Paths};
use crate::transport::{classify_console_url, UrlProblem};
use anyhow::{bail, Context, Result};
use protocol::bakery::BakedConfig;
use std::future::Future;
use tokio::sync::mpsc;

/// Where a launch begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartState {
    /// Credentials on disk: connect to the console.
    Enrolled,
    /// A token is baked into the binary: enroll silently first.
    AutoEnroll,
    /// Ask for console URL + token. `server_url` is the prefill (last known or baked console);
    /// `locked` when the binary is baked for one console and the URL must not change.
    Connect { server_url: String, locked: bool },
}

/// The start-state matrix: enrolled wins; a baked token auto-enrolls; everything else asks.
pub fn start_state(local: Option<&LocalConfig>, baked: Option<&BakedConfig>) -> StartState {
    if local.map(|c| c.is_enrolled()).unwrap_or(false) {
        return StartState::Enrolled;
    }
    match baked {
        Some(b) if b.enroll_token.is_some() => StartState::AutoEnroll,
        Some(b) => StartState::Connect {
            server_url: b.server_url.clone(),
            locked: true,
        },
        None => StartState::Connect {
            server_url: local.map(|c| c.server_url.clone()).unwrap_or_default(),
            locked: false,
        },
    }
}

/// The state a launch should act on. `requested` marks the person having asked to enroll again
/// (Settings → *Enroll again*) rather than a normal start.
///
/// A baked binary carries an enrollment token, so the silent path would put it straight back
/// into the console it came from — from the Settings screen, indistinguishable from the button
/// being dead. When asked for by hand it therefore goes to the Connect screen instead, with
/// the console it is tied to filled in and locked.
pub fn start_state_for(
    local: Option<&LocalConfig>,
    baked: Option<&BakedConfig>,
    requested: bool,
) -> StartState {
    match start_state(local, baked) {
        StartState::AutoEnroll if requested => StartState::Connect {
            server_url: baked.map(|b| b.server_url.clone()).unwrap_or_default(),
            locked: baked.is_some(),
        },
        other => other,
    }
}

/// Normalise and validate what was typed into the Console URL field. `Ok` carries the URL to
/// use (`https://` assumed when no scheme was given, trailing slash removed); `Err` is the
/// message to show under the field.
pub fn validate_console_url(input: &str) -> Result<String, String> {
    validate_console_url_with(input, crate::transport::insecure_allowed())
}

pub fn validate_console_url_with(input: &str, insecure_allowed: bool) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter the console URL".into());
    }
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let normalised = with_scheme.trim_end_matches('/').to_string();
    let parsed = url::Url::parse(&normalised).map_err(|_| "Enter a valid URL".to_string())?;
    if parsed.host_str().map(str::is_empty).unwrap_or(true) {
        return Err("Enter a valid URL".into());
    }
    match classify_console_url(&normalised, insecure_allowed) {
        Ok(_) => Ok(normalised),
        Err(UrlProblem::PlainTextPublic) => {
            Err("Use https:// — plain http is only allowed for local addresses".into())
        }
        Err(UrlProblem::UnsupportedScheme(_)) => Err("Use https://".into()),
        Err(UrlProblem::Invalid) => Err("Enter a valid URL".into()),
    }
}

/// Which console an enrollment may target.
///
/// A baked binary carries its console's URL (and its TLS pin) in the signed trailer, and it is
/// branded for that console: letting it enroll somewhere else would hand a stranger an agent
/// wearing someone's branding, pinned to a certificate that no longer matches. The Connect
/// screen locks the field for a baked build, but a lock only the page enforces is no lock, so
/// the decision lives here and the submitted URL is checked against the baked one.
///
/// Hosts are compared case-insensitively, and a URL that only differs by trailing slash or
/// default port is the same console.
pub fn enrollment_target(baked: Option<&BakedConfig>, requested: &str) -> Result<String, String> {
    let requested = validate_console_url(requested)?;
    let Some(baked_url) = baked.map(|b| b.server_url.as_str()) else {
        return Ok(requested);
    };
    let baked_url = baked_url.trim_end_matches('/').to_string();
    if same_console(&baked_url, &requested) {
        // Always the baked spelling: it is what the pin and the branding were issued for.
        return Ok(baked_url);
    }
    let host = url::Url::parse(&baked_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or(baked_url);
    Err(format!(
        "This agent was built for {host} and can only enroll there. Download an agent from the other console instead."
    ))
}

/// Whether two console URLs address the same console (scheme, host, port and path).
fn same_console(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        url::Url::parse(s).ok().map(|u| {
            (
                u.scheme().to_ascii_lowercase(),
                u.host_str().unwrap_or_default().to_ascii_lowercase(),
                u.port_or_known_default(),
                u.path().trim_end_matches('/').to_string(),
            )
        })
    };
    match (norm(a), norm(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.trim_end_matches('/') == b.trim_end_matches('/'),
    }
}

/// Default device name offered on the Connect screen: the last known display name, else the
/// hostname.
pub fn default_device_name(local: Option<&LocalConfig>) -> String {
    local
        .and_then(|c| c.cached.as_ref())
        .map(|c| c.display_name.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(hostname)
}

fn hostname() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".into())
}

/// Serve Connect-screen submissions until one enrollment succeeds. Every submission is
/// validated first (URL policy, non-empty token); `emit` receives the UI states to render.
/// Returns `None` when the request channel closes without an enrollment.
pub async fn connect_flow<F, Fut>(
    baked: Option<&BakedConfig>,
    rx: &mut mpsc::UnboundedReceiver<ConnectRequest>,
    emit: impl Fn(ConnectUi),
    mut enroll: F,
) -> Option<()>
where
    F: FnMut(ConnectRequest) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    while let Some(req) = rx.recv().await {
        let server_url = match enrollment_target(baked, &req.server_url) {
            Ok(u) => u,
            Err(message) => {
                emit(ConnectUi::Failed { message });
                continue;
            }
        };
        let token = req.token.trim().to_string();
        if token.is_empty() {
            emit(ConnectUi::Failed {
                message: "Enter the enrollment token".into(),
            });
            continue;
        }
        emit(ConnectUi::Busy);
        let name = req
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        match enroll(ConnectRequest {
            server_url,
            token,
            name,
        })
        .await
        {
            Ok(()) => {
                emit(ConnectUi::Done);
                return Some(());
            }
            Err(e) => {
                tracing::warn!("enrollment from the Connect screen failed: {e:#}");
                emit(ConnectUi::Failed {
                    message: crate::enroll::error_message(&e),
                });
            }
        }
    }
    None
}

/// Make sure credentials exist before the hub starts. Headless (no app window) behaviour is
/// unchanged: a baked token enrolls, anything else fails with the CLI hint. With the window,
/// the Connect screen is shown and this only returns once an enrollment succeeded. `notice`
/// is shown on the Connect screen (why we are here, e.g. the device was deleted).
/// Get the agent enrolled, showing the Connect screen when it cannot be done silently.
///
/// `requested` marks the person having asked for this (Settings → *Enroll again*) rather than
/// a normal start. It suppresses the silent path: a baked binary would otherwise enroll itself
/// straight back into the console it was built for with the token it carries, which from the
/// Settings screen looks like the button doing nothing at all.
pub async fn ensure_enrolled(paths: &Paths, notice: Option<String>, requested: bool) -> Result<()> {
    let local = LocalConfig::load(paths)?;
    let baked = crate::baked::get().map(|b| &b.config);
    let (prefill, locked, error) = match start_state_for(local.as_ref(), baked, requested) {
        StartState::Enrolled => return Ok(()),
        StartState::AutoEnroll => match crate::enroll::auto_enroll_if_baked(paths).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                tracing::error!("auto-enrollment failed: {e:#}");
                if !crate::app::is_running() {
                    return Err(e);
                }
                (
                    baked.map(|b| b.server_url.clone()).unwrap_or_default(),
                    true,
                    Some(crate::enroll::error_message(&e)),
                )
            }
        },
        StartState::Connect { server_url, locked } => {
            if !crate::app::is_running() {
                bail!(
                    "agent is not enrolled: run `remote-agent enroll --server <url> --token <token>`"
                );
            }
            let notice = match (locked, baked) {
                (true, Some(b)) => {
                    let host = url::Url::parse(&b.server_url)
                        .ok()
                        .and_then(|u| u.host_str().map(str::to_string))
                        .unwrap_or_else(|| b.server_url.clone());
                    let tied = format!(
                        "This agent was built for {host} and can only enroll there. To use a different console, download an agent from it."
                    );
                    Some(match notice {
                        Some(n) => format!("{n} {tied}"),
                        None => tied,
                    })
                }
                _ => notice,
            };
            (server_url, locked, notice)
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    crate::app::set_connect_handler(std::sync::Arc::new(move |req| {
        let _ = tx.send(req);
    }));
    crate::app::set_connect_ui(ConnectUi::Show {
        server_url: prefill,
        name: default_device_name(local.as_ref()),
        locked,
        error,
    });
    let paths_for_enroll = paths.clone();
    let done = connect_flow(baked, &mut rx, crate::app::set_connect_ui, move |req| {
        let paths = paths_for_enroll.clone();
        async move {
            crate::enroll::enroll(&paths, &req.server_url, &req.token, req.name)
                .await
                .map(|_| ())
        }
    })
    .await;
    crate::app::set_connect_handler(std::sync::Arc::new(|_| {}));
    done.context("the application window closed before enrollment completed")
}

/// Forget the device identity after the console rejected it (deleted device / bad credentials)
/// or the user asked to enroll again.
pub fn forget_enrollment(paths: &Paths) -> Result<()> {
    if let Some(mut local) = LocalConfig::load(paths)? {
        local.clear_enrollment();
        local.save(paths)?;
    }
    crate::secrets::forget(paths);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::config::AgentConfig;

    fn baked(token: bool) -> BakedConfig {
        BakedConfig {
            version: 1,
            server_url: "https://baked.example".into(),
            enroll_token: token.then(|| "tok".to_string()),
            quick_support: false,
            branding: Default::default(),
            issued_at: 0,
            console_tls_spki_sha256: None,
        }
    }

    fn enrolled() -> LocalConfig {
        LocalConfig {
            server_url: "https://old.example".into(),
            device_id: "dev_1".into(),
            device_secret: "s".into(),
            ..Default::default()
        }
    }

    fn unenrolled_with_url() -> LocalConfig {
        LocalConfig {
            server_url: "https://old.example".into(),
            ..Default::default()
        }
    }

    fn baked_for(url: &str) -> BakedConfig {
        BakedConfig {
            version: 1,
            server_url: url.to_string(),
            enroll_token: None,
            quick_support: false,
            branding: protocol::bakery::Branding::default(),
            issued_at: 0,
            console_tls_spki_sha256: None,
        }
    }

    #[test]
    fn enrolling_again_by_hand_shows_the_screen_instead_of_re_enrolling_silently() {
        let with_token = BakedConfig {
            enroll_token: Some("tok".into()),
            ..baked_for("https://console.example.com")
        };
        // A normal start still enrolls itself.
        assert_eq!(
            start_state_for(None, Some(&with_token), false),
            StartState::AutoEnroll
        );
        // Asked for by hand it stops and shows where it is tied to.
        assert_eq!(
            start_state_for(None, Some(&with_token), true),
            StartState::Connect {
                server_url: "https://console.example.com".into(),
                locked: true,
            }
        );
        // An unbaked agent has nothing to re-enroll with, so nothing changes.
        assert_eq!(
            start_state_for(None, None, true),
            StartState::Connect {
                server_url: String::new(),
                locked: false,
            }
        );
    }

    #[test]
    fn a_baked_agent_only_enrolls_with_the_console_it_was_built_for() {
        let baked = baked_for("https://console.example.com");
        // The same console spelled differently is still the same console.
        for spelling in [
            "https://console.example.com",
            "https://console.example.com/",
            "https://Console.Example.com",
            "console.example.com",
            "https://console.example.com:443",
        ] {
            assert_eq!(
                enrollment_target(Some(&baked), spelling).as_deref(),
                Ok("https://console.example.com"),
                "{spelling}"
            );
        }
        // Anything else is refused, and the message names where it does belong.
        let err = enrollment_target(Some(&baked), "https://other.example.net").unwrap_err();
        assert!(err.contains("console.example.com"), "{err}");
        assert!(enrollment_target(Some(&baked), "https://console.example.com.evil.net").is_err());
        // A different port is a different console.
        assert!(enrollment_target(Some(&baked), "https://console.example.com:8443").is_err());
    }

    #[test]
    fn an_unbaked_agent_may_enroll_anywhere_the_url_policy_allows() {
        assert_eq!(
            enrollment_target(None, "https://console.example.com/").as_deref(),
            Ok("https://console.example.com")
        );
        assert!(enrollment_target(None, "not a url").is_err());
    }

    #[test]
    fn start_state_matrix() {
        // enrolled → status, whatever the trailer says
        assert_eq!(start_state(Some(&enrolled()), None), StartState::Enrolled);
        assert_eq!(
            start_state(Some(&enrolled()), Some(&baked(true))),
            StartState::Enrolled
        );
        // baked token, not enrolled → silent enrollment
        assert_eq!(
            start_state(None, Some(&baked(true))),
            StartState::AutoEnroll
        );
        assert_eq!(
            start_state(Some(&unenrolled_with_url()), Some(&baked(true))),
            StartState::AutoEnroll
        );
        // baked without token → ask for a token, console fixed
        assert_eq!(
            start_state(None, Some(&baked(false))),
            StartState::Connect {
                server_url: "https://baked.example".into(),
                locked: true
            }
        );
        // plain binary → ask, prefilled with the last known console (if any)
        assert_eq!(
            start_state(None, None),
            StartState::Connect {
                server_url: String::new(),
                locked: false
            }
        );
        assert_eq!(
            start_state(Some(&unenrolled_with_url()), None),
            StartState::Connect {
                server_url: "https://old.example".into(),
                locked: false
            }
        );
        // a cleared enrollment (device deleted) is "not enrolled" again
        let mut cleared = enrolled();
        cleared.clear_enrollment();
        assert!(!cleared.is_enrolled());
        assert_eq!(
            start_state(Some(&cleared), None),
            StartState::Connect {
                server_url: "https://old.example".into(),
                locked: false
            }
        );
    }

    #[test]
    fn url_validation_messages() {
        let v = |s: &str| validate_console_url_with(s, false);
        assert_eq!(v("").unwrap_err(), "Enter the console URL");
        assert_eq!(v("   ").unwrap_err(), "Enter the console URL");
        assert_eq!(
            v("console.example.com").unwrap(),
            "https://console.example.com"
        );
        assert_eq!(
            v(" https://console.example.com/ ").unwrap(),
            "https://console.example.com"
        );
        assert_eq!(
            v("http://localhost:8080/").unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            v("http://192.168.1.10:8080").unwrap(),
            "http://192.168.1.10:8080"
        );
        assert_eq!(
            v("http://console.example.com").unwrap_err(),
            "Use https:// — plain http is only allowed for local addresses"
        );
        assert_eq!(v("ftp://console.example.com").unwrap_err(), "Use https://");
        assert_eq!(v("https://").unwrap_err(), "Enter a valid URL");
        assert_eq!(v("not a url").unwrap_err(), "Enter a valid URL");
        // the override lets plain http through
        assert_eq!(
            validate_console_url_with("http://console.example.com", true).unwrap(),
            "http://console.example.com"
        );
    }

    #[test]
    fn default_name_prefers_cached_display_name() {
        let mut c = enrolled();
        c.cached = Some(AgentConfig {
            display_name: "Front desk".into(),
            ..Default::default()
        });
        assert_eq!(default_device_name(Some(&c)), "Front desk");
        c.cached = None;
        assert!(!default_device_name(Some(&c)).is_empty());
        assert!(!default_device_name(None).is_empty());
    }

    /// Round trip of the Connect screen: submissions in, UI states out, enrollment mocked.
    #[tokio::test]
    async fn connect_flow_round_trip() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let states = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&states);
        let emit = move |ui: ConnectUi| seen.lock().push(ui);
        let attempts = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&attempts);
        let enroll = move |req: ConnectRequest| {
            let log = std::sync::Arc::clone(&log);
            async move {
                log.lock().push(req.clone());
                if req.token == "bad" {
                    Err(crate::enroll::Rejected {
                        status: 401,
                        code: "invalid_token".into(),
                        message: "invalid or expired token".into(),
                    }
                    .into())
                } else {
                    Ok(())
                }
            }
        };

        // 1. bad URL → validation error, enroll not called
        tx.send(ConnectRequest {
            server_url: "http://console.example.com".into(),
            token: "t".into(),
            name: None,
        })
        .unwrap();
        // 2. empty token → validation error
        tx.send(ConnectRequest {
            server_url: "console.example.com".into(),
            token: "  ".into(),
            name: None,
        })
        .unwrap();
        // 3. wrong token → console message verbatim
        tx.send(ConnectRequest {
            server_url: "console.example.com".into(),
            token: "bad".into(),
            name: Some(" Front desk ".into()),
        })
        .unwrap();
        // 4. good token → done
        tx.send(ConnectRequest {
            server_url: "https://console.example.com/".into(),
            token: " good ".into(),
            name: Some("".into()),
        })
        .unwrap();

        crate::transport::set_insecure(false);
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connect_flow(None, &mut rx, emit, enroll),
        )
        .await
        .unwrap();
        assert_eq!(done, Some(()));

        let states = states.lock().clone();
        assert_eq!(
            states,
            vec![
                ConnectUi::Failed {
                    message: "Use https:// — plain http is only allowed for local addresses".into()
                },
                ConnectUi::Failed {
                    message: "Enter the enrollment token".into()
                },
                ConnectUi::Busy,
                ConnectUi::Failed {
                    message: "invalid or expired token".into()
                },
                ConnectUi::Busy,
                ConnectUi::Done,
            ]
        );
        let attempts = attempts.lock().clone();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].server_url, "https://console.example.com");
        assert_eq!(attempts[0].name.as_deref(), Some("Front desk"));
        assert_eq!(attempts[1].server_url, "https://console.example.com");
        assert_eq!(attempts[1].token, "good");
        assert_eq!(attempts[1].name, None);
    }

    #[tokio::test]
    async fn connect_flow_ends_when_the_window_goes_away() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ConnectRequest>();
        drop(tx);
        let done = connect_flow(None, &mut rx, |_| {}, |_| async { Ok(()) }).await;
        assert_eq!(done, None);
    }
}
