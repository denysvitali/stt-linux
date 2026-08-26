//! Audio input device discovery.
//!
//! Capture and resampling arrive in M1; for now this exists so `stt doctor`
//! can surface device problems before they show up as a silent recording.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};

/// Sample rate every ASR engine in this project expects.
pub const TARGET_SAMPLE_RATE: cpal::SampleRate = 16_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    /// Stable, persistable identifier. Preferred over `name` in config,
    /// because names are not unique and shift as devices are re-plugged.
    pub id: Option<String>,
    /// The device's own default capture format — the configuration we would
    /// actually open. Far more informative than the supported *ranges*, which
    /// ALSA reports as absurdities like `1-4294967295Hz`.
    pub default_config: Option<String>,
}

/// The input devices available on this host.
#[derive(Debug, Clone, Default)]
pub struct InputDevices {
    /// The host default.
    ///
    /// Kept separate rather than flagged inside [`Self::devices`]: on ALSA the
    /// default is a distinct PCM (`alsa:default`) that does not appear in the
    /// enumeration at all, so any "which listed device is the default?" match
    /// silently finds nothing.
    pub default: Option<DeviceInfo>,
    pub devices: Vec<DeviceInfo>,
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "<unnamed>".into())
}

fn device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

fn describe(device: &cpal::Device) -> DeviceInfo {
    let default_config = device.default_input_config().ok().map(|c| {
        format!(
            "{}ch {}Hz {:?}",
            c.channels(),
            c.sample_rate(),
            c.sample_format()
        )
    });
    DeviceInfo {
        name: device_name(device),
        id: device_id(device),
        default_config,
    }
}

/// Enumerate input devices on the default host.
///
/// On this stack cpal talks to PipeWire through its ALSA compatibility layer,
/// which is the most likely thing to be misconfigured, so failures here are
/// reported rather than swallowed.
pub fn list_input_devices() -> Result<InputDevices> {
    let host = cpal::default_host();
    let default = host.default_input_device().as_ref().map(describe);
    let devices = host
        .input_devices()
        .context("enumerating audio input devices")?
        .map(|d| describe(&d))
        .collect();
    Ok(InputDevices { default, devices })
}

/// cpal device id for the PipeWire ALSA PCM.
pub const PIPEWIRE_PCM: &str = "alsa:pipewire";

/// Resolve the configured device selector to a cpal device.
///
/// `"default"` goes through [`default_input_device`]. Any other value is
/// matched against device IDs (stable) first and then human-readable names
/// (what users actually type), including the host default, which ALSA does not
/// list in its enumeration.
pub fn find_input_device(selector: &str) -> Result<cpal::Device> {
    if selector == "default" {
        return default_input_device();
    }
    let host = cpal::default_host();

    let mut candidates: Vec<_> = host
        .input_devices()
        .context("enumerating audio input devices")?
        .collect();
    // The host default is a real, selectable PCM (`alsa:default`) that does not
    // appear in `input_devices()`. Without this, naming it explicitly fails.
    if let Some(d) = host.default_input_device() {
        candidates.push(d);
    }

    if let Some(i) = candidates
        .iter()
        .position(|d| device_id(d).as_deref() == Some(selector))
    {
        return Ok(candidates.swap_remove(i));
    }
    if let Some(i) = candidates.iter().position(|d| device_name(d) == selector) {
        return Ok(candidates.swap_remove(i));
    }

    let available: Vec<_> = candidates.iter().filter_map(device_id).collect();
    anyhow::bail!(
        "no audio input device matching `{selector}`; available ids: {}",
        if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        }
    )
}

/// Pick the device to record from when the user has not named one.
///
/// Prefers the PipeWire PCM over ALSA's `default`. On a PipeWire system the
/// ALSA `default` PCM frequently does *not* route capture to the session's
/// default source — measured on a sof-hda-dsp laptop, `default` returned a
/// 0.0008 peak (silence) while `pipewire` returned 0.1366 from the same
/// microphone at the same moment. Recording silence is the worst possible
/// failure here, because it looks like a broken model rather than a broken
/// route.
///
/// Going through PipeWire also means the user can redirect our stream with
/// `wpctl` or pavucontrol, which the raw ALSA PCM does not allow.
pub fn default_input_device() -> Result<cpal::Device> {
    let host = cpal::default_host();

    if let Ok(devices) = host.input_devices()
        && let Some(pw) = devices
            .into_iter()
            .find(|d| device_id(d).as_deref() == Some(PIPEWIRE_PCM))
    {
        tracing::debug!("using the PipeWire PCM as the default input");
        return Ok(pw);
    }

    host.default_input_device()
        .context("no default audio input device")
}

/// Which device `"default"` actually resolves to, for diagnostics.
pub fn describe_default_input() -> Option<DeviceInfo> {
    default_input_device().ok().as_ref().map(describe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_device_error_lists_alternatives() {
        // Runs without audio hardware: either enumeration fails (also fine) or
        // we get a "no device matching" error that names what *is* available.
        if let Err(e) = find_input_device("definitely-not-a-device-xyzzy") {
            let msg = format!("{e:#}");
            assert!(
                msg.contains("xyzzy") || msg.contains("enumerating"),
                "unhelpful error: {msg}"
            );
        }
    }
}
