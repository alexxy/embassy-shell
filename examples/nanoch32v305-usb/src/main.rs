//! embassy-shell over USB CDC-ACM on a NanoCH32V305 (CH32V305RBT6).
//!
//! The shell runs on the USB serial port (PA12 = DP, PA11 = DM, USB OTG FS).
//! Open any terminal program on the "Embassy shell device" COM port, 115200
//! (baud rate is ignored on CDC). The shell respawns every time the host
//! (re)connects the port.
//! Commands: `led on|off|toggle|blink [period_ms]`, `pwm <0-255>|off`,
//! `status`.
//!
//! Note: on the NanoCH32V305 the OTG port is shared with the ISP boot
//! pins; if the board does not enumerate, make sure it is not held in
//! the bootloader.
//!
//! * onboard LED: PA3 (active low)
//! * PWM on the same PA3 pin: PA3 = TIM2_CH4 (no remap), hardware PWM at
//!   1 kHz with active-low polarity, so the user brightness 0..255 maps
//!   linearly onto the timer duty and the LED is lit during the "on" time.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use core::sync::atomic::{AtomicU32, Ordering};

use ch32_hal as hal;
use ch32_hal::otg_fs::{self, Driver};
use ch32_hal::time::Hertz;
use ch32_hal::timer::low_level::{CountingMode, OutputPolarity};
use ch32_hal::timer::simple_pwm::{PwmPin, SimplePwm};
use ch32_hal::timer::Channel as PwmChannel;
use ch32_hal::usb::EndpointDataBuffer512;
use ch32_hal::{bind_interrupts, peripherals};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{select, Either};
use embassy_shell::Shell;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State};
use embedded_alloc::LlffHeap;
use panic_halt as _;

defmt::timestamp!("{=u32}", 0u32);
/// defmt's `assert!`/`panic!` require the `_defmt_panic` symbol; keep halting
/// like `panic-halt` does.
#[defmt::panic_handler]
fn defmt_panic() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

// The shell's heap working set is bounded (max_line_len / max_history
// caps), so a modest pool is enough even for worst-case input.
const HEAP_SIZE: usize = 8 * 1024;

#[global_allocator]
static HEAP: LlffHeap = LlffHeap::empty();
static mut HEAP_MEM: [u8; HEAP_SIZE] = [0; HEAP_SIZE];

bind_interrupts!(struct Irqs {
    OTG_FS => otg_fs::InterruptHandler<peripherals::OTG_FS>;
});

#[derive(Clone, Copy)]
enum LedCmd {
    On,
    Off,
    Toggle,
    Blink(u32),
}

static LED_CH: Channel<CriticalSectionRawMutex, LedCmd, 8> = Channel::new();
/// Half-period of `led blink` in milliseconds; 0 = not blinking.
static LED_PERIOD_MS: AtomicU32 = AtomicU32::new(0);
/// Current user brightness, 0..=255 (0 = off).
static LED_BRIGHTNESS: AtomicU32 = AtomicU32::new(0);
static PWM_SIG: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Set LED brightness (0..=255); applied by `pwm_task` on the next signal.
fn set_brightness(value: u8) {
    LED_BRIGHTNESS.store(u32::from(value), Ordering::Relaxed);
    PWM_SIG.signal(());
}

fn build_shell() -> Shell<'static> {
    let mut shell = Shell::new();
    shell.prompt("ch32v305-usb> ");
    shell.max_history(16);

    shell.add_command_with_options(
        "led",
        "led on|off|toggle|blink [period_ms]",
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
                    defmt::info!("led {}", args.get(0).unwrap_or(""));
                    LED_CH.send(cmd).await;
                }
                Ok(())
            })
        },
    );

    shell.add_command(
        "pwm",
        "pwm <0-255>|off   (PA3 = TIM2_CH4, 1 kHz)",
        |args, mut io| {
            Box::pin(async move {
                match args.get(0) {
                    None => {
                        let b = LED_BRIGHTNESS.load(Ordering::Relaxed);
                        io.println(&format!("pwm brightness: {b}/255")).await
                    }
                    Some("off") => {
                        set_brightness(0);
                        defmt::info!("pwm off");
                        io.println("pwm off").await
                    }
                    Some(s) => match s.parse::<u8>() {
                        Ok(v) => {
                            set_brightness(v);
                            defmt::info!("pwm brightness {}", v);
                            io.println(&format!("pwm brightness: {v}/255")).await
                        }
                        Err(_) => io.println("usage: pwm <0-255>|off").await,
                    },
                }
            })
        },
    );

    shell.add_command("status", "uptime, led and pwm state", |_args, mut io| {
        Box::pin(async move {
            let up = embassy_time::Instant::now().as_secs();
            let period = LED_PERIOD_MS.load(Ordering::Relaxed);
            let brightness = LED_BRIGHTNESS.load(Ordering::Relaxed);
            let led = if period > 0 {
                "blinking"
            } else if brightness > 0 {
                "on"
            } else {
                "off"
            };
            io.println(&format!(
                "uptime: {} s | led: {} | brightness: {}/255",
                up, led, brightness
            ))
            .await
        })
    });

    shell
}

#[embassy_executor::task]
async fn led_task() {
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
        }
        LED_PERIOD_MS.store(period, Ordering::Relaxed);
        set_brightness(if on { 255 } else { 0 });

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
                        set_brightness(if on { 255 } else { 0 });
                    }
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn pwm_task(mut pwm: SimplePwm<'static, peripherals::TIM2>) {
    // The LED is active low, so run the channel with active-low polarity:
    // a bigger duty means more time with the pin pulled low, i.e. more
    // light. The channel stays enabled; duty 0 just keeps the pin at its
    // inactive (high) level.
    pwm.set_polarity(PwmChannel::Ch4, OutputPolarity::ActiveLow);
    pwm.set_duty(PwmChannel::Ch4, 0);
    pwm.enable(PwmChannel::Ch4);
    let max = pwm.get_max_duty();
    loop {
        PWM_SIG.wait().await;
        let b = LED_BRIGHTNESS.load(Ordering::Relaxed);
        pwm.set_duty(PwmChannel::Ch4, (max * b) / 255);
    }
}

#[derive(Debug, Clone, Copy)]
struct CdcError;

impl core::fmt::Display for CdcError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("usb cdc error")
    }
}

impl core::error::Error for CdcError {}

impl embedded_io::Error for CdcError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::NotConnected
    }
}

/// Byte-stream reader on top of the CDC-ACM bulk OUT endpoint.
/// An endpoint error (e.g. the host went away) is reported as EOF.
struct CdcReader<'a, 'd, D: embassy_usb::driver::Driver<'d>> {
    rx: &'a mut Receiver<'d, D>,
    buf: [u8; 64],
    start: usize,
    end: usize,
}

impl<'d, D: embassy_usb::driver::Driver<'d>> embedded_io::ErrorType for CdcReader<'_, 'd, D> {
    type Error = CdcError;
}

impl<'d, D: embassy_usb::driver::Driver<'d>> embedded_io_async::Read for CdcReader<'_, 'd, D> {
    async fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.start < self.end {
                let n = core::cmp::min(out.len(), self.end - self.start);
                out[..n].copy_from_slice(&self.buf[self.start..self.start + n]);
                self.start += n;
                return Ok(n);
            }
            match self.rx.read_packet(&mut self.buf).await {
                Ok(n) => {
                    self.start = 0;
                    self.end = n;
                }
                // Disconnected: report EOF so that `Shell::run` exits cleanly.
                Err(_) => return Ok(0),
            }
        }
    }
}

/// Byte-stream writer on top of the CDC-ACM bulk IN endpoint.
struct CdcWriter<'a, 'd, D: embassy_usb::driver::Driver<'d>> {
    tx: &'a mut Sender<'d, D>,
}

impl<'d, D: embassy_usb::driver::Driver<'d>> embedded_io::ErrorType for CdcWriter<'_, 'd, D> {
    type Error = CdcError;
}

impl<'d, D: embassy_usb::driver::Driver<'d>> embedded_io_async::Write for CdcWriter<'_, 'd, D> {
    async fn write(&mut self, data: &[u8]) -> Result<usize, Self::Error> {
        if data.is_empty() {
            return Ok(0);
        }
        let mps = self.tx.max_packet_size() as usize;
        let n = core::cmp::min(data.len(), mps);
        self.tx
            .write_packet(&data[..n])
            .await
            .map_err(|_| CdcError)?;
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[embassy_executor::main(entry = "qingke_rt::entry")]
async fn main(spawner: Spawner) -> ! {
    // USB needs a 144 MHz core clock derived from the HSI.
    let cfg = hal::Config {
        rcc: hal::rcc::Config::SYSCLK_FREQ_144MHZ_HSI,
        ..Default::default()
    };
    let p = hal::init(cfg);
    defmt::info!("embassy-shell on CH32V305 (usb)");

    let heap_ptr = core::ptr::addr_of_mut!(HEAP_MEM);
    unsafe { HEAP.init(heap_ptr as usize, HEAP_SIZE) };

    // The LED and the PWM share PA3; brightness starts at 0 (led off).
    spawner.spawn(led_task().unwrap());

    spawner.spawn(
        pwm_task(SimplePwm::new(
            p.TIM2,
            None,
            None,
            None,
            Some(PwmPin::new_ch4::<0>(p.PA3)),
            Hertz(1_000),
            CountingMode::default(),
        ))
        .unwrap(),
    );

    // --- USB CDC-ACM ---
    let mut epbuf: [EndpointDataBuffer512; 4] =
        core::array::from_fn(|_| EndpointDataBuffer512::default());
    let driver = Driver::new(p.OTG_FS, p.PA12, p.PA11, &mut epbuf);

    let mut usb_config = embassy_usb::Config::new(0xc0de, 0xcafe);
    usb_config.manufacturer = Some("embassy-shell");
    usb_config.product = Some("Embassy shell device");
    usb_config.max_power = 100;
    usb_config.max_packet_size_0 = 64;

    let mut config_descriptor = [0u8; 256];
    let mut bos_descriptor = [0u8; 256];
    let mut msos_descriptor = [0u8; 256];
    let mut control_buf = [0u8; 64];

    let mut state = State::default();
    let mut builder = embassy_usb::Builder::new(
        driver,
        usb_config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );

    let class = CdcAcmClass::new(&mut builder, &mut state, 64);
    let (mut tx, mut rx) = class.split();

    let mut dev = builder.build();

    join(dev.run(), async {
        loop {
            tx.wait_connection().await;
            rx.wait_connection().await;
            defmt::info!("shell session");
            let mut shell = build_shell();
            let mut reader = CdcReader {
                rx: &mut rx,
                buf: [0u8; 64],
                start: 0,
                end: 0,
            };
            let mut writer = CdcWriter { tx: &mut tx };
            // EOF (host closed the port) ends `run`; wait for a
            // reconnect and start a fresh shell.
            let _ = shell.run(&mut reader, &mut writer).await;
        }
    })
    .await;

    loop {
        Timer::after_secs(3600).await;
    }
}
