#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_time::Timer;
use embassy_usb::class::hid::{self, HidBootProtocol, HidSubclass, HidWriter, State};
use embassy_usb::{Builder, Config};
use usbd_hid::descriptor::{KeyboardReport, SerializedDescriptor};
// Linked for their side effects only: defmt logging over RTT and the panic handler.
use {defmt_rtt as _, panic_probe as _};

// Route the USB controller interrupt to the embassy-rp USB driver.
bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

/// Number of matrix columns (driven outputs).
const COLS: usize = 5;
/// Number of matrix rows (sensed inputs).
const ROWS: usize = 5;

/// HID keycode for each matrix position, indexed as `[column][row]`.
/// `0x00` means there is no key at that position.
///
/// Codes are from the HID Usage Tables, Keyboard/Keypad page.
const KEYMAP: [[u8; ROWS]; COLS] = [
    [0x53, 0x5F, 0x5C, 0x59, 0x62],
    [0x54, 0x60, 0x5D, 0x5A, 0x00],
    [0x55, 0x61, 0x5E, 0x5B, 0x63],
    [0x56, 0x57, 0x00, 0x58, 0x00],
    [0x00, 0x00, 0x00, 0x00, 0x00],
];

#[embassy_executor::main(executor = "embassy_rp::executor::Executor", entry = "cortex_m_rt::entry")]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // --- Describe the USB device ---
    let driver = Driver::new(p.USB, Irqs);

    // Test VID/PID from the Embassy examples.
    let mut config = Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("manu042");
    config.product = Some("Pico Numpad");
    // Maximum bus current in mA that the device announces to the host.
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Buffers the USB stack uses for its descriptors and control transfers.
    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut msos_descriptor = [0; 256];
    let mut control_buf = [0; 64];
    let mut state = State::new();

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );

    // --- Register the HID keyboard interface ---
    let hid_config = hid::Config {
        // Standard boot-keyboard report: modifiers, reserved byte, 6 keycodes.
        report_descriptor: KeyboardReport::desc(),
        request_handler: None,
        // How often the host polls for a new report, in milliseconds.
        poll_ms: 10,
        // A keyboard report is exactly 8 bytes.
        max_packet_size: 8,
        // Boot protocol lets the keyboard work in BIOS/UEFI setup screens too.
        hid_subclass: HidSubclass::Boot,
        hid_boot_protocol: HidBootProtocol::Keyboard,
    };
    let mut writer = HidWriter::<_, 8>::new(&mut builder, &mut state, hid_config);

    let mut usb = builder.build();
    // Handles enumeration and all bus traffic; must be polled for as long as
    // the device is running.
    let usb_fut = usb.run();

    // --- Key matrix ---
    // The diodes conduct from column to row, so the columns are driven and
    // the rows are read.

    // Columns: outputs, idle low. One column at a time is pulled high.
    let mut cols: [Output; COLS] = [
        Output::new(p.PIN_6, Level::Low),
        Output::new(p.PIN_7, Level::Low),
        Output::new(p.PIN_8, Level::Low),
        Output::new(p.PIN_9, Level::Low),
        Output::new(p.PIN_10, Level::Low),
    ];
    // Rows: inputs with pull-down. A row reads high when a key in the
    // currently active column is pressed.
    let rows: [Input; ROWS] = [
        Input::new(p.PIN_11, Pull::Down),
        Input::new(p.PIN_12, Pull::Down),
        Input::new(p.PIN_13, Pull::Down),
        Input::new(p.PIN_14, Pull::Down),
        Input::new(p.PIN_15, Pull::Down),
    ];

    let key_fut = async {
        // Keycodes of the report that was sent last.
        let mut last = [0u8; 6];

        loop {
            // Collect all keys that are currently pressed. A boot-keyboard
            // report holds at most 6 keys; any further keys are dropped.
            let mut keycodes = [0u8; 6];
            let mut n = 0;

            for (c, col) in cols.iter_mut().enumerate() {
                col.set_high();
                // Give the line a moment to settle before reading the rows.
                Timer::after_micros(10).await;
                for (r, row) in rows.iter().enumerate() {
                    let code = KEYMAP[c][r];
                    if row.is_high() && code != 0 && n < 6 {
                        keycodes[n] = code;
                        n += 1;
                    }
                }
                col.set_low();
            }

            // Only send a report when the set of pressed keys has changed.
            // A release is reported the same way: the key is simply missing
            // from the next report.
            if keycodes != last {
                last = keycodes;
                let report = KeyboardReport {
                    // Bit field for Ctrl/Shift/Alt/GUI; unused on a numpad.
                    modifier: 0,
                    reserved: 0,
                    leds: 0,
                    keycodes,
                };
                // A failed write (e.g. USB not connected) is ignored; the
                // next change sends a fresh report anyway.
                let _ = writer.write_serialize(&report).await;
            }

            // Scan interval. It also acts as a crude debounce; there is no
            // dedicated debouncing yet.
            Timer::after_millis(5).await;
        }
    };

    // Run the USB stack and the matrix scan concurrently; neither returns.
    join(usb_fut, key_fut).await;
}
