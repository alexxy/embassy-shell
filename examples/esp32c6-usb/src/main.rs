//! embassy-shell over the USB Serial/JTAG peripheral of an ESP32-C6.
//!
//! The ESP32-C6 has no USB-OTG controller: its "USB console" is the built-in
//! Serial/JTAG peripheral exposed on the `USB` connector (the same port used
//! for flashing). Open it with any COM terminal.
//!
//! Commands: `rgb <r> <g> <b>|<color>|off`, `led on|off|toggle|blink [period_ms]`,
//! `status`.
//!
//! * WS2812 RGB LED on GPIO8 (D8 of the ESP32-C6 Supermini), driven by RMT
//!   TX channel 0 with an 80 MHz tick. `led` operates on the last color set
//!   with `rgb` (white by default).
//!
//! NOTE: firmware use of the Serial/JTAG port conflicts with probe-rs's
//! debug access while the firmware runs; flash with `--connect-under-reset`
//! (already set in `.cargo/config.toml`). This is also why `defmt` is an
//! optional (off-by-default) feature here.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_shell::Shell;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::Level;
use esp_hal::peripherals;
use esp_hal::rmt::{Channel as RmtChannel, PulseCode, Rmt, Tx, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx};
use esp_hal::Async;
use panic_halt as _;

#[cfg(feature = "defmt")]
use defmt_rtt as _;
#[cfg(feature = "defmt")]
defmt::timestamp!("{=u32}", 0u32);
/// defmt's `assert!`/`panic!` require the `_defmt_panic` symbol; keep halting
/// like `panic-halt` does.
#[cfg(feature = "defmt")]
#[defmt::panic_handler]
fn defmt_panic() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

// Logging goes through defmt (RTT) when the `defmt` feature is enabled and
// is compiled out otherwise.
#[cfg(feature = "defmt")]
macro_rules! log {
    ($($arg:tt)*) => { defmt::info!($($arg)*); };
}
#[cfg(not(feature = "defmt"))]
macro_rules! log {
    ($($arg:tt)*) => {};
}

esp_bootloader_esp_idf::esp_app_desc!();

#[derive(Clone, Copy)]
enum LedCmd {
    On,
    Off,
    Toggle,
    Blink(u32),
    /// Set the color (`0x00RRGGBB`) and turn on.
    Color(u32),
}

static LED_CH: Channel<CriticalSectionRawMutex, LedCmd, 8> = Channel::new();
/// Last color requested via `rgb`, packed as `0x00RRGGBB`.
static RGB_COLOR: AtomicU32 = AtomicU32::new(0x00FF_FFFF);
/// Whether the strip currently shows its color.
static RGB_IS_ON: AtomicBool = AtomicBool::new(false);
/// Half-period of `led blink` in milliseconds; 0 = not blinking.
static LED_PERIOD_MS: AtomicU32 = AtomicU32::new(0);

// WS2812 bit timings at the 80 MHz RMT tick (12.5 ns/tick):
// T0H/T0L ~= 400/850 ns, T1H/T1L ~= 800/450 ns.
const T0_HIGH: u16 = 32;
const T0_LOW: u16 = 68;
const T1_HIGH: u16 = 64;
const T1_LOW: u16 = 36;

fn preset_color(name: &str) -> Option<u32> {
    match name {
        "red" => Some(0x00FF_0000),
        "green" => Some(0x0000_FF00),
        "blue" => Some(0x0000_00FF),
        "yellow" => Some(0x00FF_FF00),
        "magenta" => Some(0x00FF_00FF),
        "cyan" => Some(0x0000_FFFF),
        "white" => Some(0x00FF_FFFF),
        _ => None,
    }
}

fn fmt_color(c: u32) -> alloc::string::String {
    format!("{} {} {}", (c >> 16) & 0xFF, (c >> 8) & 0xFF, c & 0xFF)
}

fn build_shell() -> Shell<'static> {
    let mut shell = Shell::new();
    shell.prompt("esp32c6> ");
    shell.max_history(16);

    shell.add_command_with_options(
        "rgb",
        "rgb <r> <g> <b>|<color>|off   (WS2812 on GPIO8/D8)",
        &[
            "off",
            "red",
            "green",
            "blue",
            "yellow",
            "magenta",
            "cyan",
            "white",
        ],
        |args, mut io| {
            Box::pin(async move {
                let cmd = match args.get(0) {
                    None => {
                        io.println(&format!(
                            "rgb color: {} (off)",
                            fmt_color(RGB_COLOR.load(Ordering::Relaxed))
                        ))
                        .await?;
                        None
                    }
                    Some("off") => Some(LedCmd::Off),
                    Some(s) if preset_color(s).is_some() => Some(LedCmd::Color(preset_color(s).unwrap())),
                    Some(s) => {
                        let r = s.parse::<u32>();
                        let g = args.get(1).and_then(|a| a.parse::<u32>().ok());
                        let b = args.get(2).and_then(|a| a.parse::<u32>().ok());
                        match (r, g, b) {
                            (Ok(r), Some(g), Some(b)) if r <= 255 && g <= 255 && b <= 255 => {
                                Some(LedCmd::Color((r << 16) | (g << 8) | b))
                            }
                            _ => {
                                io.println(
                                    "usage: rgb <r> <g> <b> (0-255) | red|green|blue|yellow|magenta|cyan|white | off",
                                )
                                .await?;
                                None
                            }
                        }
                    }
                };
                if let Some(cmd) = cmd {
                    match cmd {
                        LedCmd::Color(c) => {
                            io.println(&format!("rgb: {}", fmt_color(c))).await?;
                        }
                        LedCmd::Off => io.println("rgb off").await?,
                        _ => (),
                    }
                    LED_CH.send(cmd).await;
                }
                Ok(())
            })
        },
    );

    shell.add_command_with_options(
        "led",
        "led on|off|toggle|blink [period_ms]   (shows current rgb color)",
        &["on", "off", "toggle", "blink"],
        |args, mut io| {
            Box::pin(async move {
                let cmd = match args.get(0) {
                    Some("on") => Some(LedCmd::On),
                    Some("off") => Some(LedCmd::Off),
                    Some("toggle") => Some(LedCmd::Toggle),
                    Some("blink") => match args.get(1) {
                        None => Some(LedCmd::Blink(500)),
                        Some(s) => match s.parse::<u32>() {
                            Ok(ms) if (10..=60_000).contains(&ms) => Some(LedCmd::Blink(ms)),
                            _ => {
                                io.println("usage: led blink [period_ms, 10..60000]")
                                    .await?;
                                None
                            }
                        },
                    },
                    _ => {
                        io.println("usage: led on|off|toggle|blink [period_ms]")
                            .await?;
                        None
                    }
                };
                if let Some(cmd) = cmd {
                    LED_CH.send(cmd).await;
                }
                Ok(())
            })
        },
    );

    shell.add_command("status", "uptime and rgb state", |_args, mut io| {
        Box::pin(async move {
            let up = embassy_time::Instant::now().as_secs();
            let period = LED_PERIOD_MS.load(Ordering::Relaxed);
            let state = if period > 0 {
                "blinking"
            } else if RGB_IS_ON.load(Ordering::Relaxed) {
                "on"
            } else {
                "off"
            };
            io.println(&format!(
                "uptime: {} s | rgb: {} | color: {}",
                up,
                state,
                fmt_color(RGB_COLOR.load(Ordering::Relaxed))
            ))
            .await
        })
    });

    shell
}

/// Latch one WS2812 frame: `color` when `on`, black otherwise.
async fn show(channel: &mut RmtChannel<'static, Async, Tx>, on: bool) {
    let color = if on {
        RGB_COLOR.load(Ordering::Relaxed)
    } else {
        0
    };
    let r = (color >> 16) as u8;
    let g = (color >> 8) as u8;
    let b = color as u8;

    // The strip wants GRB order, MSB first, then an end marker.
    let mut codes = [PulseCode::end_marker(); 25];
    let mut i = 0;
    for byte in [g, r, b] {
        for shift in (0..8).rev() {
            codes[i] = if (byte >> shift) & 1 == 1 {
                PulseCode::new(Level::High, T1_HIGH, Level::Low, T1_LOW)
            } else {
                PulseCode::new(Level::High, T0_HIGH, Level::Low, T0_LOW)
            };
            i += 1;
        }
    }

    // `_e` is only consumed by the optional defmt log below.
    if let Err(_e) = channel.transmit(&codes).await {
        log!("ws2812 transmit error: {:?}", _e);
    }
}

#[embassy_executor::task]
async fn rgb_task(rmt_periph: peripherals::RMT<'static>, pin: peripherals::GPIO8<'static>) {
    // The RMT driver types are not `Send`, so everything is built inside
    // the task itself.
    let rmt = Rmt::new(rmt_periph, Rate::from_mhz(80))
        .unwrap()
        .into_async();
    // Keep the line high while idle: a >50 us high gap is the strip's
    // latch/reset condition.
    let tx_config = TxChannelConfig::default()
        .with_idle_output(true)
        .with_idle_output_level(Level::High);
    let mut channel = rmt.channel0.configure_tx(&tx_config).unwrap().with_pin(pin);

    let mut on = false;
    let mut period;
    let mut cmd = LedCmd::Off;
    loop {
        match cmd {
            LedCmd::On => {
                on = true;
                period = 0;
            }
            LedCmd::Off => {
                on = false;
                period = 0;
            }
            LedCmd::Toggle => {
                on = !on;
                period = 0;
            }
            LedCmd::Blink(ms) => {
                on = true;
                period = ms;
            }
            LedCmd::Color(c) => {
                RGB_COLOR.store(c, Ordering::Relaxed);
                on = true;
                period = 0;
            }
        }
        LED_PERIOD_MS.store(period, Ordering::Relaxed);
        RGB_IS_ON.store(on, Ordering::Relaxed);
        show(&mut channel, on).await;

        if period == 0 {
            // Block until the next command arrives.
            cmd = LED_CH.receive().await;
        } else {
            // Blink until the next command arrives (receive is cancel-safe).
            loop {
                match select(LED_CH.receive(), Timer::after_millis(period as u64 / 2)).await {
                    Either::First(new_cmd) => {
                        cmd = new_cmd;
                        break;
                    }
                    Either::Second(_) => {
                        on = !on;
                        RGB_IS_ON.store(on, Ordering::Relaxed);
                        show(&mut channel, on).await;
                    }
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn shell_task(
    mut rx: UsbSerialJtagRx<'static, Async>,
    mut tx: UsbSerialJtagTx<'static, Async>,
) {
    let _ = embedded_io_async::Write::write_all(
        &mut tx,
        b"\r\nembassy-shell on ESP32-C6 (USB Serial/JTAG)\r\n",
    )
    .await;
    loop {
        let mut shell = build_shell();
        // EOF ends `run`; just respawn.
        let _ = shell.run(&mut rx, &mut tx).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let p = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);

    let timg0 = TimerGroup::new(p.TIMG0);
    let sw = esp_hal::interrupt::software::SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw.software_interrupt0);

    let usb = UsbSerialJtag::new(p.USB_DEVICE).into_async();
    let (rx, tx) = usb.split();

    spawner.spawn(rgb_task(p.RMT, p.GPIO8).unwrap());
    spawner.spawn(shell_task(rx, tx).unwrap());

    log!("embassy-shell started on USB Serial/JTAG");

    loop {
        Timer::after_secs(3600).await;
    }
}
