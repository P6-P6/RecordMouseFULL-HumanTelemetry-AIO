//! Snapshot of the Windows pointer-ballistics configuration.
//!
//! WHY THIS EXISTS
//! ---------------
//! `raw_dx/dy` (what the sensor reported) and `cursor_x/y` (where the pointer
//! ended up) are related by a transform Windows applies: a sensitivity divisor,
//! and -- when "Enhance pointer precision" is on -- a piecewise-linear
//! acceleration curve read from the registry and scaled by screen DPI and
//! refresh rate.
//!
//! Every comparable project on GitHub records only the cursor layer, which is
//! why their datasets are silently machine-specific: replay the same motor
//! signal on a box with different sensitivity and you get different pixels. By
//! storing the curve alongside the raw counts, the transform becomes invertible
//! offline and the motor signal stays portable.
//!
//! Every field here is DIRECTLY OBSERVED -- read back from Windows, not
//! inferred.

use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETMOUSE, SPI_GETMOUSESPEED,
};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Ballistics {
    /// Slider position, 1..=20. 10 is the 1:1 default; each step below halves
    /// toward 1/8, each step above scales up to 3.5x.
    pub pointer_speed: i32,
    /// `SPI_GETMOUSE`: [threshold1, threshold2, acceleration_enabled].
    /// The legacy pre-Vista doubling/quadrupling thresholds.
    pub mouse_threshold1: i32,
    pub mouse_threshold2: i32,
    /// Non-zero when "Enhance pointer precision" is enabled. When this is 0 the
    /// transform is the plain sensitivity divisor and the curves below are
    /// inert.
    pub enhance_pointer_precision: i32,
    /// `HKCU\Control Panel\Mouse\SmoothMouseXCurve`, raw bytes.
    /// 5 points x 8 bytes; each point is a 16.16 fixed-point pair.
    pub smooth_mouse_x_curve: Vec<u8>,
    pub smooth_mouse_y_curve: Vec<u8>,
    /// Decoded curve points as floats, for convenience in analysis.
    /// x = input speed (counts/ms), y = output scaling.
    pub curve_x: Vec<f64>,
    pub curve_y: Vec<f64>,
    /// String values Windows keeps in parallel with the SPI values. Recorded
    /// verbatim because they occasionally disagree with the SPI readout after
    /// third-party tools touch them.
    pub reg_sensitivity: Option<String>,
    pub reg_speed: Option<String>,
    pub reg_threshold1: Option<String>,
    pub reg_threshold2: Option<String>,
}

impl Ballistics {
    pub fn snapshot() -> Self {
        let mut b = Self::default();

        // SAFETY: SPI_GETMOUSESPEED writes one i32 through pvParam.
        unsafe {
            SystemParametersInfoW(
                SPI_GETMOUSESPEED,
                0,
                &mut b.pointer_speed as *mut i32 as *mut c_void,
                0,
            );
        }

        // SAFETY: SPI_GETMOUSE writes exactly three i32 through pvParam.
        let mut m = [0i32; 3];
        unsafe {
            SystemParametersInfoW(SPI_GETMOUSE, 0, m.as_mut_ptr() as *mut c_void, 0);
        }
        b.mouse_threshold1 = m[0];
        b.mouse_threshold2 = m[1];
        b.enhance_pointer_precision = m[2];

        b.smooth_mouse_x_curve = read_binary(r"Control Panel\Mouse", "SmoothMouseXCurve");
        b.smooth_mouse_y_curve = read_binary(r"Control Panel\Mouse", "SmoothMouseYCurve");
        b.curve_x = decode_curve(&b.smooth_mouse_x_curve);
        b.curve_y = decode_curve(&b.smooth_mouse_y_curve);

        b.reg_sensitivity = read_string(r"Control Panel\Mouse", "MouseSensitivity");
        b.reg_speed = read_string(r"Control Panel\Mouse", "MouseSpeed");
        b.reg_threshold1 = read_string(r"Control Panel\Mouse", "MouseThreshold1");
        b.reg_threshold2 = read_string(r"Control Panel\Mouse", "MouseThreshold2");

        b
    }

    /// True when the raw -> cursor transform is a plain scalar, which makes
    /// offline inversion exact rather than approximate.
    pub fn is_linear(&self) -> bool {
        self.enhance_pointer_precision == 0
    }
}

/// Decode a SmoothMouse curve blob into f64 pairs.
///
/// Layout is 5 entries of 8 bytes. Each entry is two little-endian u32s that
/// together form a 16.16 fixed-point value: the low u32 is the fractional
/// part, the high u32 the integer part. Undocumented by Microsoft but stable
/// since Vista and consistent across every machine this has been checked on.
fn decode_curve(raw: &[u8]) -> Vec<f64> {
    let mut out = Vec::new();
    for chunk in raw.chunks_exact(8) {
        let frac = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
        let int = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        out.push(int as f64 + (frac as f64) / 65_536.0);
    }
    out
}

fn open(subkey: &str) -> Option<HKEY> {
    let wide: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call;
    // `key` receives the opened handle.
    let rc = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, wide.as_ptr(), 0, KEY_READ, &mut key)
    };
    if rc == 0 {
        Some(key)
    } else {
        None
    }
}

fn read_raw(subkey: &str, value: &str) -> Option<(u32, Vec<u8>)> {
    let key = open(subkey)?;
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut ty: u32 = 0;
    let mut len: u32 = 0;

    // SAFETY: null data pointer asks for the required byte count.
    let rc = unsafe {
        RegQueryValueExW(key, wide.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut len)
    };
    if rc != 0 || len == 0 || len > 65_536 {
        // SAFETY: `key` came from a successful RegOpenKeyExW.
        unsafe { RegCloseKey(key) };
        return None;
    }

    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` holds `len` bytes as just reported by the sizing call.
    let rc = unsafe {
        RegQueryValueExW(key, wide.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut len)
    };
    // SAFETY: `key` came from a successful RegOpenKeyExW and is not used again.
    unsafe { RegCloseKey(key) };

    if rc != 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some((ty, buf))
}

fn read_binary(subkey: &str, value: &str) -> Vec<u8> {
    read_raw(subkey, value).map(|(_, b)| b).unwrap_or_default()
}

fn read_string(subkey: &str, value: &str) -> Option<String> {
    let (_, bytes) = read_raw(subkey, value)?;
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    Some(String::from_utf16_lossy(&wide))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_windows_default_x_curve() {
        // The stock SmoothMouseXCurve shipped by Windows.
        let raw: [u8; 40] = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //  0.0
            0x15, 0x6E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //  0.43
            0x00, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, //  1.25
            0x29, 0xDC, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, //  3.86
            0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00, // 40.0
        ];
        let c = decode_curve(&raw);
        assert_eq!(c.len(), 5);
        assert_eq!(c[0], 0.0);
        assert!((c[4] - 40.0).abs() < 1e-9, "got {}", c[4]);
        // Curve must be monotonically non-decreasing to be a sane transform.
        for w in c.windows(2) {
            assert!(w[1] >= w[0]);
        }
    }

    #[test]
    fn tolerates_a_missing_or_truncated_curve() {
        assert!(decode_curve(&[]).is_empty());
        assert_eq!(decode_curve(&[1, 2, 3]).len(), 0); // partial entry ignored
    }

    #[test]
    fn snapshot_reads_a_plausible_live_configuration() {
        let b = Ballistics::snapshot();
        // Windows constrains the slider to 1..=20; anything else means we read
        // the wrong thing.
        assert!(
            (1..=20).contains(&b.pointer_speed),
            "pointer_speed out of range: {}",
            b.pointer_speed
        );
    }
}
