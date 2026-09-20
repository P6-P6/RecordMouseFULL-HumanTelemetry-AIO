//! Raw Input device enumeration and identity.
//!
//! Every event carries a `dev` index into this table rather than a raw HANDLE,
//! because HANDLEs are not stable across replug and mean nothing after the
//! session ends. The table itself is serialised into the session header, so a
//! dataset recorded months ago still says which physical mouse produced it.
//!
//! The table is owned exclusively by the capture thread. `WM_INPUT_DEVICE_CHANGE`
//! is delivered to the same thread as `WM_INPUT`, so hotplug rebuilds need no
//! synchronisation at all.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use windows_sys::Win32::Devices::HumanInterfaceDevice::HID_USAGE_GENERIC_MOUSE;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::UI::Input::{
    GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RID_DEVICE_INFO,
    RIDI_DEVICEINFO, RIDI_DEVICENAME, RIM_TYPEMOUSE,
};

/// What we can honestly learn about one physical mouse.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct DeviceInfo {
    /// Index used by `Event::dev`.
    pub index: u8,
    /// Win32 device interface path, e.g. `\\?\HID#VID_046D&PID_C52B&...`.
    /// Stable across reboots for the same port/device, unlike the HANDLE.
    pub name: String,
    /// Parsed out of `name` when present. USB vendor/product ids.
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    /// From `RID_DEVICE_INFO_MOUSE`. Windows reports these; they are not
    /// probed or guessed.
    pub buttons: u32,
    /// Device-reported sample rate in Hz. This is the driver's claim, which is
    /// frequently 0 or a nominal value rather than the true polling rate --
    /// treat as a hint. The measured inter-event interval in the data is the
    /// authoritative number.
    pub reported_sample_rate: u32,
    pub has_horizontal_wheel: bool,
    /// True for the synthetic entry that absorbs events from a device that
    /// vanished mid-session.
    pub synthetic: bool,
}

pub struct DeviceTable {
    devices: Vec<DeviceInfo>,
    by_handle: HashMap<isize, u8>,
}

/// Index handed to events whose device we could not identify. Always present
/// so an unknown device can never cause an event to be dropped.
pub const UNKNOWN_DEVICE: u8 = 0;

impl DeviceTable {
    pub fn new() -> Self {
        let mut t = Self { devices: Vec::new(), by_handle: HashMap::new() };
        t.devices.push(DeviceInfo {
            index: UNKNOWN_DEVICE,
            name: "<unknown>".into(),
            synthetic: true,
            ..Default::default()
        });
        t.refresh();
        t
    }

    /// Resolve a HANDLE from `RAWINPUTHEADER.hDevice` to a table index.
    ///
    /// Hot path: one hash lookup. A miss means a device appeared without us
    /// seeing the notification yet; the event is still recorded, attributed to
    /// `UNKNOWN_DEVICE`, and the next refresh picks the device up properly.
    #[inline]
    pub fn index_of(&self, handle: HANDLE) -> u8 {
        *self.by_handle.get(&(handle as isize)).unwrap_or(&UNKNOWN_DEVICE)
    }

    pub fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }

    /// Re-enumerate. Existing indices are preserved (matched by device path) so
    /// that events recorded before a replug still point at the same device.
    pub fn refresh(&mut self) -> bool {
        let found = enumerate_mice();
        let mut changed = false;
        self.by_handle.clear();

        for (handle, mut info) in found {
            match self.devices.iter().position(|d| d.name == info.name && !d.synthetic) {
                Some(pos) => {
                    // Known device, possibly a new HANDLE after replug.
                    self.by_handle.insert(handle as isize, self.devices[pos].index);
                }
                None => {
                    if self.devices.len() >= 255 {
                        // Vanishingly unlikely; refuse rather than wrap a u8.
                        continue;
                    }
                    info.index = self.devices.len() as u8;
                    self.by_handle.insert(handle as isize, info.index);
                    self.devices.push(info);
                    changed = true;
                }
            }
        }
        changed
    }
}

impl Default for DeviceTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Enumerate every Raw Input device of type mouse.
fn enumerate_mice() -> Vec<(HANDLE, DeviceInfo)> {
    let mut out = Vec::new();
    let stride = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
    let mut count: u32 = 0;

    // SAFETY: the documented two-call pattern -- a null buffer asks for the
    // required count, which is then allocated before the second call.
    let rc = unsafe { GetRawInputDeviceList(std::ptr::null_mut(), &mut count, stride) };
    if rc == u32::MAX || count == 0 {
        return out;
    }

    let mut list: Vec<RAWINPUTDEVICELIST> = vec![unsafe { std::mem::zeroed() }; count as usize];
    // SAFETY: `list` holds `count` elements of exactly `stride` bytes.
    let n = unsafe { GetRawInputDeviceList(list.as_mut_ptr(), &mut count, stride) };
    if n == u32::MAX {
        return out;
    }

    for entry in list.iter().take(n as usize) {
        if entry.dwType != RIM_TYPEMOUSE {
            continue;
        }
        let name = device_name(entry.hDevice).unwrap_or_default();
        let (vendor_id, product_id) = parse_vid_pid(&name);
        let mut info = DeviceInfo {
            index: 0,
            name,
            vendor_id,
            product_id,
            ..Default::default()
        };
        if let Some(d) = device_caps(entry.hDevice) {
            // SAFETY: dwType was RIM_TYPEMOUSE, so the mouse arm of the union
            // is the initialised one.
            let m = unsafe { d.Anonymous.mouse };
            info.buttons = m.dwNumberOfButtons;
            info.reported_sample_rate = m.dwSampleRate;
            info.has_horizontal_wheel = m.fHasHorizontalWheel != 0;
        }
        out.push((entry.hDevice, info));
    }
    out
}

fn device_name(h: HANDLE) -> Option<String> {
    let mut len: u32 = 0;
    // SAFETY: null buffer queries the required length in WCHARs.
    unsafe { GetRawInputDeviceInfoW(h, RIDI_DEVICENAME, std::ptr::null_mut(), &mut len) };
    if len == 0 || len > 4096 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: buffer holds `len` WCHARs as just reported.
    let n = unsafe {
        GetRawInputDeviceInfoW(h, RIDI_DEVICENAME, buf.as_mut_ptr() as *mut _, &mut len)
    };
    if n == u32::MAX {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn device_caps(h: HANDLE) -> Option<RID_DEVICE_INFO> {
    // SAFETY: zeroed is a valid starting state; cbSize must be set by us.
    let mut info: RID_DEVICE_INFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<RID_DEVICE_INFO>() as u32;
    let mut size = info.cbSize;
    // SAFETY: `info` is a correctly sized RID_DEVICE_INFO with cbSize set.
    let rc = unsafe {
        GetRawInputDeviceInfoW(h, RIDI_DEVICEINFO, &mut info as *mut _ as *mut _, &mut size)
    };
    if rc == u32::MAX {
        None
    } else {
        Some(info)
    }
}

/// Pull USB ids out of a device interface path.
///
/// Example: `\\?\HID#VID_046D&PID_C52B&MI_01&Col01#7&1f2a...` -> (0x046D, 0xC52B).
/// Bluetooth and PS/2 devices legitimately have neither, hence `Option`.
fn parse_vid_pid(name: &str) -> (Option<u16>, Option<u16>) {
    fn field(s: &str, key: &str) -> Option<u16> {
        let i = s.find(key)? + key.len();
        let hex: String = s[i..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() < 4 {
            return None;
        }
        u16::from_str_radix(&hex[..4], 16).ok()
    }
    let upper = name.to_ascii_uppercase();
    (field(&upper, "VID_"), field(&upper, "PID_"))
}

/// The HID usage pair for a generic desktop mouse, used at registration.
pub const USAGE_PAGE_GENERIC: u16 = 0x01;
pub const USAGE_MOUSE: u16 = HID_USAGE_GENERIC_MOUSE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usb_ids_from_a_real_device_path() {
        let p = r"\\?\HID#VID_046D&PID_C52B&MI_01&Col01#7&1f2a3b4c&0&0000#{378de44c}";
        assert_eq!(parse_vid_pid(p), (Some(0x046D), Some(0xC52B)));
    }

    #[test]
    fn handles_devices_with_no_usb_ids() {
        assert_eq!(parse_vid_pid(r"\\?\ACPI#PNP0F03#4&1234#{378de44c}"), (None, None));
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(parse_vid_pid(r"\\?\hid#vid_1532&pid_0084#6&abc"), (Some(0x1532), Some(0x0084)));
    }

    #[test]
    fn unknown_device_always_resolves() {
        // A table that has never seen a handle must still attribute events
        // somewhere rather than dropping them.
        let t = DeviceTable { devices: vec![DeviceInfo::default()], by_handle: HashMap::new() };
        assert_eq!(t.index_of(0x1234 as HANDLE), UNKNOWN_DEVICE);
    }
}
