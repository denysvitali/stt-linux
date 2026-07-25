//! Shared vocabulary for backend availability checks.

/// How usable a backend is in the current session.
///
/// The middle state matters: a backend that *partly* works (evdev that can
/// open some devices but maybe not the keyboard) must not show up as a clean
/// tick, or `stt doctor` teaches users to trust a check that then fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// Present but not fully verified, or working with caveats.
    Degraded,
    Unavailable,
}

impl Availability {
    /// Whether the daemon should be willing to select this backend.
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Available | Self::Degraded)
    }

    /// Glyph for terminal output.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Available => "\x1b[32m✓\x1b[0m",
            Self::Degraded => "\x1b[33m!\x1b[0m",
            Self::Unavailable => "\x1b[31m✗\x1b[0m",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degraded_is_still_selectable() {
        assert!(Availability::Available.is_usable());
        assert!(Availability::Degraded.is_usable());
        assert!(!Availability::Unavailable.is_usable());
    }
}
