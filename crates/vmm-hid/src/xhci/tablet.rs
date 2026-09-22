// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! USB HID tablet (absolute pointer) device.
//!
//! The device has 3 buttons, a 2-axis absolute pointer (16-bit X/Y, range
//! 0..0x7FFF) and a scroll wheel. The VNC server calls
//! [`TabletDevice::pointer_event`] when the remote client moves the pointer
//! or presses a button. The xHCI controller then sends the report to the
//! guest through the interrupt IN endpoint.

use std::sync::Mutex;

use super::bits;

// ---- USB descriptors (const byte arrays) ---------------------------------

/// Device descriptor (18 bytes).
const DEVICE_DESC: [u8; 18] = [
    18,                  // bLength
    bits::USB_DT_DEVICE, // bDescriptorType
    0x00,
    0x02, // bcdUSB = 2.00
    0x00, // bDeviceClass (defined at interface level)
    0x00, // bDeviceSubClass
    0x00, // bDeviceProtocol
    64,   // bMaxPacketSize0
    0x5D,
    0xFB, // idVendor = 0xFB5D (little-endian)
    0x01,
    0x00, // idProduct = 0x0001
    0x00,
    0x01, // bcdDevice = 1.00
    1,    // iManufacturer (string index)
    2,    // iProduct (string index)
    0,    // iSerialNumber
    1,    // bNumConfigurations
];

/// Configuration descriptor + interface + HID + endpoint (total 34 bytes).
const CONFIG_DESC: [u8; 34] = [
    // Configuration descriptor (9 bytes)
    9,                   // bLength
    bits::USB_DT_CONFIG, // bDescriptorType
    34,
    0,    // wTotalLength = 34
    1,    // bNumInterfaces
    1,    // bConfigurationValue
    0,    // iConfiguration
    0xA0, // bmAttributes (bus-powered, remote wakeup)
    50,   // bMaxPower (100 mA)
    // Interface descriptor (9 bytes)
    9,                      // bLength
    bits::USB_DT_INTERFACE, // bDescriptorType
    0,                      // bInterfaceNumber
    0,                      // bAlternateSetting
    1,                      // bNumEndpoints
    0x03,                   // bInterfaceClass (HID)
    0x00,                   // bInterfaceSubClass (no boot)
    0x00,                   // bInterfaceProtocol (none)
    0,                      // iInterface
    // HID descriptor (9 bytes)
    9,                // bLength
    bits::USB_DT_HID, // bDescriptorType
    0x10,
    0x01,                    // bcdHID = 1.10
    0,                       // bCountryCode
    1,                       // bNumDescriptors
    bits::USB_DT_HID_REPORT, // bDescriptorType (report)
    // wDescriptorLength = length of HID_REPORT_DESC
    HID_REPORT_DESC_LEN as u8,
    (HID_REPORT_DESC_LEN >> 8) as u8,
    // Endpoint descriptor (7 bytes)
    7,                     // bLength
    bits::USB_DT_ENDPOINT, // bDescriptorType
    0x81,                  // bEndpointAddress (EP1 IN)
    0x03,                  // bmAttributes (interrupt)
    8,
    0,  // wMaxPacketSize = 8
    10, // bInterval (10 ms)
];

/// HID Report Descriptor for absolute pointer tablet.
///
/// Usage Page: Generic Desktop
///   Usage: Mouse
///     Collection: Application
///       Usage: Pointer
///         Collection: Physical
///           Usage Page: Button (1-3)
///           Usage Page: Generic Desktop (X, Y absolute 0..0x7FFF)
///           Usage Page: Generic Desktop (Wheel relative -127..127)
///         End Collection
///     End Collection
const HID_REPORT_DESC: [u8; 84] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    // Buttons (3 buttons, 1 bit each + 5 padding bits)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (Button 1)
    0x29, 0x03, //     Usage Maximum (Button 3)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x03, //     Report Count (3)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Var, Abs)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x05, //     Report Size (5) -- padding
    0x81, 0x01, //     Input (Const, Array, Abs)
    // X axis (absolute, 16-bit, 0..0x7FFF)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x35, 0x00, //     Physical Minimum (0)
    0x46, 0xFF, 0x7F, //     Physical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x02, //     Input (Data, Var, Abs)
    // Y axis (absolute, 16-bit, 0..0x7FFF)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x35, 0x00, //     Physical Minimum (0)
    0x46, 0xFF, 0x7F, //     Physical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x02, //     Input (Data, Var, Abs)
    // Scroll wheel (relative, 8-bit, -127..127)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Var, Rel)
    0xC0, //   End Collection
    0xC0, // End Collection
];

const HID_REPORT_DESC_LEN: usize = HID_REPORT_DESC.len();

/// Size of the HID report: buttons(1) + X(2) + Y(2) + wheel(1) = 6 bytes.
pub const REPORT_SIZE: usize = 6;

// ---- String descriptors --------------------------------------------------

/// String descriptor 0: language ID (US English 0x0409).
const STRING_DESC_0: [u8; 4] = [4, bits::USB_DT_STRING, 0x09, 0x04];

/// Build a UTF-16LE string descriptor from an ASCII string.
fn make_string_desc(s: &str) -> Vec<u8> {
    let mut desc = Vec::with_capacity(2 + s.len() * 2);
    desc.push(0); // placeholder for bLength
    desc.push(bits::USB_DT_STRING);
    for b in s.bytes() {
        desc.push(b);
        desc.push(0);
    }
    desc[0] = desc.len() as u8;
    desc
}

// ---- Tablet device state -------------------------------------------------

struct TabletState {
    /// Current button state (bits 0-2).
    buttons: u8,
    /// Current X position (0..0x7FFF).
    x: u16,
    /// Current Y position (0..0x7FFF).
    y: u16,
    /// Pending report ready to send.
    report_pending: bool,
    /// USB address assigned by SET_ADDRESS.
    address: u8,
    /// Current configuration (0 = unconfigured, 1 = configured).
    configuration: u8,
    /// HID idle rate (in 4ms units, 0 = infinite).
    idle_rate: u8,
}

/// USB HID absolute pointer tablet device.
///
/// Thread-safe: the VNC server thread calls `pointer_event` while the
/// xHCI controller reads reports from the vCPU thread. Internal state
/// is protected by a mutex with short critical sections.
pub struct TabletDevice {
    inner: Mutex<TabletState>,
    /// Pre-built string descriptors.
    string_descs: [Vec<u8>; 3],
}

impl Default for TabletDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl TabletDevice {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(TabletState {
                buttons: 0,
                x: 0,
                y: 0,
                report_pending: false,
                address: 0,
                configuration: 0,
                idle_rate: 0,
            }),
            string_descs: [
                STRING_DESC_0.to_vec(),
                make_string_desc("Triton VMM"),
                make_string_desc("USB Tablet"),
            ],
        }
    }

    /// Called by the VNC server when the remote client moves the pointer
    /// or changes button state.
    ///
    /// Coordinates are absolute in the range [0, 0x7FFF].
    pub fn pointer_event(&self, buttons: u8, x: u16, y: u16) {
        let mut st = self.inner.lock().expect("tablet lock");
        st.buttons = buttons & 0x07; // only 3 buttons
        st.x = x.min(0x7FFF);
        st.y = y.min(0x7FFF);
        st.report_pending = true;
    }

    pub fn has_pending_report(&self) -> bool {
        let st = self.inner.lock().expect("tablet lock");
        st.report_pending
    }

    /// Build the current HID report and clear the pending flag.
    ///
    /// Returns the 6-byte report: [buttons, x_lo, x_hi, y_lo, y_hi, wheel].
    pub fn get_report(&self) -> [u8; REPORT_SIZE] {
        let mut st = self.inner.lock().expect("tablet lock");
        st.report_pending = false;
        let x_bytes = st.x.to_le_bytes();
        let y_bytes = st.y.to_le_bytes();
        [
            st.buttons, x_bytes[0], x_bytes[1], y_bytes[0], y_bytes[1],
            0, // wheel (not implemented, always 0)
        ]
    }

    /// Handle a USB control transfer (SETUP stage).
    ///
    /// `bm_request_type`, `b_request`, `w_value`, `w_index`, `w_length`
    /// are the fields from the 8-byte SETUP packet.
    ///
    /// Returns `Some(data)` for IN transfers (data to send to host),
    /// or `Some(empty)` for successful OUT/no-data transfers,
    /// or `None` to STALL.
    pub fn handle_control(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        _w_index: u16,
        w_length: u16,
    ) -> Option<Vec<u8>> {
        let dir_in = (bm_request_type & bits::USB_DIR_IN) != 0;
        let req_type = bm_request_type & 0x60; // bits 6:5
        let recipient = bm_request_type & 0x1F;

        match (req_type, b_request) {
            // Standard device requests
            (bits::USB_TYPE_STANDARD, bits::USB_REQ_GET_DESCRIPTOR)
                if dir_in =>
            {
                let desc_type = (w_value >> 8) as u8;
                let desc_index = (w_value & 0xFF) as u8;
                self.get_descriptor(desc_type, desc_index, w_length)
            }

            (bits::USB_TYPE_STANDARD, bits::USB_REQ_SET_ADDRESS) => {
                let addr = (w_value & 0x7F) as u8;
                let mut st = self.inner.lock().expect("tablet lock");
                st.address = addr;
                Some(Vec::new())
            }

            (bits::USB_TYPE_STANDARD, bits::USB_REQ_SET_CONFIGURATION) => {
                let config = (w_value & 0xFF) as u8;
                let mut st = self.inner.lock().expect("tablet lock");
                st.configuration = config;
                Some(Vec::new())
            }

            (bits::USB_TYPE_STANDARD, bits::USB_REQ_GET_CONFIGURATION)
                if dir_in =>
            {
                let st = self.inner.lock().expect("tablet lock");
                Some(vec![st.configuration])
            }

            (bits::USB_TYPE_STANDARD, bits::USB_REQ_GET_STATUS) if dir_in => {
                // Return 2 bytes of zero (self-powered=0, remote-wakeup=0)
                Some(vec![0, 0])
            }

            (bits::USB_TYPE_STANDARD, bits::USB_REQ_CLEAR_FEATURE) => {
                // Accept but ignore
                Some(Vec::new())
            }

            (bits::USB_TYPE_STANDARD, bits::USB_REQ_SET_FEATURE) => {
                // Accept but ignore
                Some(Vec::new())
            }

            // HID class requests (to interface)
            (bits::USB_TYPE_CLASS, bits::USB_REQ_HID_SET_IDLE) => {
                let idle = (w_value >> 8) as u8;
                let mut st = self.inner.lock().expect("tablet lock");
                st.idle_rate = idle;
                Some(Vec::new())
            }

            (bits::USB_TYPE_CLASS, bits::USB_REQ_HID_GET_IDLE) if dir_in => {
                let st = self.inner.lock().expect("tablet lock");
                Some(vec![st.idle_rate])
            }

            (bits::USB_TYPE_CLASS, bits::USB_REQ_HID_SET_PROTOCOL) => {
                // Accept. The device supports only the report protocol.
                Some(Vec::new())
            }

            (bits::USB_TYPE_CLASS, bits::USB_REQ_HID_GET_REPORT) if dir_in => {
                let report = self.get_report();
                let len = w_length.min(REPORT_SIZE as u16) as usize;
                Some(report[..len].to_vec())
            }

            // HID report descriptor request on interface
            (bits::USB_TYPE_STANDARD, bits::USB_REQ_GET_DESCRIPTOR)
                if dir_in && recipient == bits::USB_RECIP_INTERFACE =>
            {
                let desc_type = (w_value >> 8) as u8;
                if desc_type == bits::USB_DT_HID_REPORT {
                    let len =
                        w_length.min(HID_REPORT_DESC.len() as u16) as usize;
                    Some(HID_REPORT_DESC[..len].to_vec())
                } else {
                    None
                }
            }

            _ => {
                // Unknown request: STALL
                None
            }
        }
    }

    /// Handle GET_DESCRIPTOR for device-level descriptors.
    fn get_descriptor(
        &self,
        desc_type: u8,
        desc_index: u8,
        w_length: u16,
    ) -> Option<Vec<u8>> {
        let max_len = w_length as usize;

        match desc_type {
            bits::USB_DT_DEVICE => {
                let len = max_len.min(DEVICE_DESC.len());
                Some(DEVICE_DESC[..len].to_vec())
            }
            bits::USB_DT_CONFIG => {
                let len = max_len.min(CONFIG_DESC.len());
                Some(CONFIG_DESC[..len].to_vec())
            }
            bits::USB_DT_STRING => {
                let idx = desc_index as usize;
                if idx < self.string_descs.len() {
                    let desc = &self.string_descs[idx];
                    let len = max_len.min(desc.len());
                    Some(desc[..len].to_vec())
                } else {
                    // Unknown string index: return empty string descriptor
                    Some(vec![2, bits::USB_DT_STRING])
                }
            }
            bits::USB_DT_HID_REPORT => {
                let len = max_len.min(HID_REPORT_DESC.len());
                Some(HID_REPORT_DESC[..len].to_vec())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_desc_length() {
        assert_eq!(DEVICE_DESC.len(), 18);
        assert_eq!(DEVICE_DESC[0], 18);
    }

    #[test]
    fn device_desc_vendor_product() {
        // idVendor at offset 8-9 (LE)
        let vendor = u16::from_le_bytes([DEVICE_DESC[8], DEVICE_DESC[9]]);
        assert_eq!(vendor, 0xFB5D);
        // idProduct at offset 10-11 (LE)
        let product = u16::from_le_bytes([DEVICE_DESC[10], DEVICE_DESC[11]]);
        assert_eq!(product, 0x0001);
    }

    #[test]
    fn config_desc_total_length() {
        assert_eq!(CONFIG_DESC.len(), 34);
        let total = u16::from_le_bytes([CONFIG_DESC[2], CONFIG_DESC[3]]);
        assert_eq!(total, 34);
    }

    #[test]
    fn config_desc_contains_hid_report_len() {
        // HID descriptor is at offset 18 in CONFIG_DESC.
        // wDescriptorLength is at offset 25-26 within CONFIG_DESC.
        let hid_desc_start = 18; // config(9) + interface(9)
        let report_len_offset = hid_desc_start + 7;
        let report_len = u16::from_le_bytes([
            CONFIG_DESC[report_len_offset],
            CONFIG_DESC[report_len_offset + 1],
        ]);
        assert_eq!(report_len as usize, HID_REPORT_DESC.len());
    }

    #[test]
    fn hid_report_desc_size() {
        assert_eq!(HID_REPORT_DESC.len(), 84);
    }

    #[test]
    fn report_size() {
        assert_eq!(REPORT_SIZE, 6);
    }

    #[test]
    fn pointer_event_clamps_coordinates() {
        let tablet = TabletDevice::new();
        tablet.pointer_event(0xFF, 0xFFFF, 0xFFFF);
        let report = tablet.get_report();
        assert_eq!(report[0], 0x07);
        let x = u16::from_le_bytes([report[1], report[2]]);
        assert_eq!(x, 0x7FFF);
        let y = u16::from_le_bytes([report[3], report[4]]);
        assert_eq!(y, 0x7FFF);
    }

    #[test]
    fn pointer_event_sets_pending() {
        let tablet = TabletDevice::new();
        assert!(!tablet.has_pending_report());
        tablet.pointer_event(0, 100, 200);
        assert!(tablet.has_pending_report());
    }

    #[test]
    fn get_report_clears_pending() {
        let tablet = TabletDevice::new();
        tablet.pointer_event(1, 0x1000, 0x2000);
        assert!(tablet.has_pending_report());
        let report = tablet.get_report();
        assert!(!tablet.has_pending_report());
        assert_eq!(report[0], 1);
        let x = u16::from_le_bytes([report[1], report[2]]);
        assert_eq!(x, 0x1000);
    }

    #[test]
    fn handle_get_device_descriptor() {
        let tablet = TabletDevice::new();
        let result = tablet.handle_control(0x80, 6, 0x0100, 0, 18);
        assert!(result.is_some());
        let data = result.expect("descriptor");
        assert_eq!(data.len(), 18);
        assert_eq!(data[0], 18);
    }

    #[test]
    fn handle_get_config_descriptor() {
        let tablet = TabletDevice::new();
        let result = tablet.handle_control(0x80, 6, 0x0200, 0, 255);
        assert!(result.is_some());
        let data = result.expect("descriptor");
        assert_eq!(data.len(), 34);
    }

    #[test]
    fn handle_get_hid_report_descriptor() {
        let tablet = TabletDevice::new();
        // Request HID report descriptor (type 0x22)
        let result = tablet.handle_control(0x80, 6, 0x2200, 0, 255);
        assert!(result.is_some());
        let data = result.expect("descriptor");
        assert_eq!(data.len(), HID_REPORT_DESC.len());
    }

    #[test]
    fn handle_set_address() {
        let tablet = TabletDevice::new();
        let result = tablet.handle_control(0x00, 5, 0x0002, 0, 0);
        assert!(result.is_some());
        let st = tablet.inner.lock().expect("lock");
        assert_eq!(st.address, 2);
    }

    #[test]
    fn handle_set_configuration() {
        let tablet = TabletDevice::new();
        let result = tablet.handle_control(0x00, 9, 0x0001, 0, 0);
        assert!(result.is_some());
        let st = tablet.inner.lock().expect("lock");
        assert_eq!(st.configuration, 1);
    }

    #[test]
    fn handle_hid_set_idle() {
        let tablet = TabletDevice::new();
        let result = tablet.handle_control(0x21, 0x0A, 0x0800, 0, 0);
        assert!(result.is_some());
        let st = tablet.inner.lock().expect("lock");
        assert_eq!(st.idle_rate, 8);
    }

    #[test]
    fn handle_unknown_request_stalls() {
        let tablet = TabletDevice::new();
        let result = tablet.handle_control(0x80, 0xFF, 0, 0, 0);
        assert!(result.is_none());
    }

    #[test]
    fn string_descriptor_0_is_language() {
        let tablet = TabletDevice::new();
        let result = tablet.handle_control(0x80, 6, 0x0300, 0, 255);
        assert!(result.is_some());
        let data = result.expect("string 0");
        assert_eq!(data.len(), 4);
        assert_eq!(data[2], 0x09);
        assert_eq!(data[3], 0x04);
    }

    #[test]
    fn string_descriptor_product() {
        let tablet = TabletDevice::new();
        let result = tablet.handle_control(0x80, 6, 0x0302, 0, 255);
        assert!(result.is_some());
        let data = result.expect("string 2");
        assert!(data.len() >= 2);
        assert_eq!(data[1], bits::USB_DT_STRING);
        // "USB Tablet" in UTF-16LE: U=0x55, S=0x53, B=0x42, ...
        assert_eq!(data[2], b'U');
        assert_eq!(data[3], 0);
    }
}
