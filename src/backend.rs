use std::ffi::OsString;

use anyhow::{Result, bail};
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Backend {
    Auto,
    Wayland,
    X11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayBackend {
    Wayland,
    X11,
}

impl DisplayBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
        }
    }
}

impl Backend {
    pub fn resolve(self) -> Result<DisplayBackend> {
        self.resolve_with(|name| std::env::var_os(name))
    }

    fn resolve_with(self, get: impl Fn(&str) -> Option<OsString>) -> Result<DisplayBackend> {
        match self {
            Self::Wayland => return Ok(DisplayBackend::Wayland),
            Self::X11 => return Ok(DisplayBackend::X11),
            Self::Auto => {}
        }
        // Session type takes precedence over display variables inherited from
        // an earlier session. In particular, DISPLAY also exists on XWayland.
        let session_type = get("XDG_SESSION_TYPE");
        match session_type.as_deref().and_then(|value| value.to_str()) {
            Some("wayland") => return Ok(DisplayBackend::Wayland),
            Some("x11") => return Ok(DisplayBackend::X11),
            _ => {}
        }
        let present = |name| get(name).is_some_and(|value| !value.is_empty());
        if present("WAYLAND_DISPLAY") || present("WAYLAND_SOCKET") {
            return Ok(DisplayBackend::Wayland);
        }
        if present("DISPLAY") {
            return Ok(DisplayBackend::X11);
        }
        bail!(
            "cannot detect a graphical session; run inside your Wayland or X11 session, or select --backend wayland or --backend x11 explicitly"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(backend: Backend, variables: &[(&str, &str)]) -> Result<DisplayBackend> {
        backend.resolve_with(|name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    #[test]
    fn explicit_backend_overrides_session_hints() {
        assert_eq!(
            resolve(Backend::X11, &[("XDG_SESSION_TYPE", "wayland")]).unwrap(),
            DisplayBackend::X11
        );
        assert_eq!(
            resolve(Backend::Wayland, &[("XDG_SESSION_TYPE", "x11")]).unwrap(),
            DisplayBackend::Wayland
        );
    }

    #[test]
    fn wayland_session_never_selects_xwayland_as_a_fallback() {
        assert_eq!(
            resolve(
                Backend::Auto,
                &[("XDG_SESSION_TYPE", "wayland"), ("DISPLAY", ":0")]
            )
            .unwrap(),
            DisplayBackend::Wayland
        );
    }

    #[test]
    fn x11_session_ignores_stale_wayland_variables() {
        assert_eq!(
            resolve(
                Backend::Auto,
                &[
                    ("XDG_SESSION_TYPE", "x11"),
                    ("WAYLAND_DISPLAY", "old-wayland"),
                    ("WAYLAND_SOCKET", "42"),
                    ("DISPLAY", ":1")
                ]
            )
            .unwrap(),
            DisplayBackend::X11
        );
    }

    #[test]
    fn missing_or_unknown_session_type_uses_native_display_hints() {
        for session_type in ["", "tty", "unspecified", "other"] {
            assert_eq!(
                resolve(
                    Backend::Auto,
                    &[
                        ("XDG_SESSION_TYPE", session_type),
                        ("WAYLAND_DISPLAY", "wayland-1"),
                        ("DISPLAY", ":0")
                    ]
                )
                .unwrap(),
                DisplayBackend::Wayland
            );
            assert_eq!(
                resolve(
                    Backend::Auto,
                    &[("XDG_SESSION_TYPE", session_type), ("DISPLAY", ":0")]
                )
                .unwrap(),
                DisplayBackend::X11
            );
        }
        assert_eq!(
            resolve(Backend::Auto, &[("WAYLAND_SOCKET", "42")]).unwrap(),
            DisplayBackend::Wayland
        );
    }

    #[test]
    fn empty_display_values_do_not_select_a_backend() {
        assert!(
            resolve(
                Backend::Auto,
                &[
                    ("WAYLAND_DISPLAY", ""),
                    ("WAYLAND_SOCKET", ""),
                    ("DISPLAY", "")
                ]
            )
            .is_err()
        );
        assert!(resolve(Backend::Auto, &[]).is_err());
    }
}
