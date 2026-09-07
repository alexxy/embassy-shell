//! embassy-shell over UART0 on an ESP32-C3.
//!
//! Console: UART0, GPIO21 = TX, GPIO20 = RX, 115200 8N1.
//! Commands: `led on|off|toggle|blink [period_ms]`, `pwm <0-255>|off`,
//! `status`.
//!
//! * onboard blue LED: GPIO8 (active high)
//! * PWM on the same GPIO8 pin: LEDC low-speed channel 1 at 1 kHz (10-bit
//!   duty), so the user brightness 0..255 maps linearly onto the timer duty.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_shell::Shell;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::DriveMode;
use esp_hal::ledc::channel::{self, ChannelHW, ChannelIFace};
use esp_hal::ledc::timer::{self, TimerIFace};
use esp_hal::ledc::{LSGlobalClkSource, Ledc, LowSpeed};
use esp_hal::peripherals;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, Uart, UartRx, UartTx};
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
}

static LED_CH: Channel<CriticalSectionRawMutex, LedCmd, 8> = Channel::new();
/// Half-period of `led blink` in milliseconds; 0 = not blinking.
static LED_PERIOD_MS: AtomicU32 = AtomicU32::new(0);
/// Current user brightness, 0..=255 (0 = off).
static LED_BRIGHTNESS: AtomicU32 = AtomicU32::new(0);
static PWM_SIG: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Full-scale LEDC duty for the 10-bit timer configured in `pwm_task`.
const DUTY_MAX: u32 = (1 << 10) - 1;

/// Set LED brightness (0..=255); applied by `pwm_task` on the next signal.
fn set_brightness(value: u8) {
    LED_BRIGHTNESS.store(u32::from(value), Ordering::Relaxed);
    PWM_SIG.signal(());
}

fn build_shell() -> Shell<'static> {
    let mut shell = Shell::new();
    shell.prompt("esp32c3> ");
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
                    LED_CH.send(cmd).await;
                }
                Ok(())
            })
        },
    );

    shell.add_command(
        "pwm",
        "pwm <0-255>|off   (GPIO8 = LEDC ch1, 1 kHz)",
        |args, mut io| {
            Box::pin(async move {
                match args.get(0) {
                    None => {
                        let b = LED_BRIGHTNESS.load(Ordering::Relaxed);
                        io.println(&format!("pwm brightness: {b}/255")).await
                    }
                    Some("off") => {
                        set_brightness(0);
                        io.println("pwm off").await
                    }
                    Some(s) => match s.parse::<u8>() {
                        Ok(v) => {
                            set_brightness(v);
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
async fn pwm_task(ledc_periph: peripherals::LEDC<'static>, pin: peripherals::GPIO8<'static>) {
    // The LEDC driver types are not `Send`, so everything is built inside
    // the task itself.
    let mut ledc = Ledc::new(ledc_periph);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

    let mut lstimer = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    lstimer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty10Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_hz(1_000),
        })
        .unwrap();

    let mut channel = ledc.channel(channel::Number::Channel1, pin);
    channel
        .configure(channel::config::Config {
            timer: &lstimer,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .unwrap();

    // The LED is active high, so a bigger duty simply means more light; the
    // channel stays enabled and duty 0 keeps the pin low (led off).
    loop {
        PWM_SIG.wait().await;
        let b = LED_BRIGHTNESS.load(Ordering::Relaxed);
        channel.set_duty_hw(DUTY_MAX * b / 255);
        log!("pwm brightness set to {}/255", b);
    }
}

#[embassy_executor::task]
async fn shell_task(mut rx: UartRx<'static, Async>, mut tx: UartTx<'static, Async>) {
    let _ =
        embedded_io_async::Write::write_all(&mut tx, b"\r\nembassy-shell on ESP32-C3 (UART0)\r\n")
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

    let uart = Uart::new(p.UART0, UartConfig::default().with_baudrate(115_200))
        .unwrap()
        .with_rx(p.GPIO20)
        .with_tx(p.GPIO21)
        .into_async();
    let (rx, tx) = uart.split();

    // The LED and the PWM share GPIO8; brightness starts at 0 (led off).
    spawner.spawn(led_task().unwrap());
    spawner.spawn(pwm_task(p.LEDC, p.GPIO8).unwrap());
    spawner.spawn(shell_task(rx, tx).unwrap());

    log!("embassy-shell started on UART0 (TX=GPIO21, RX=GPIO20)");

    loop {
        Timer::after_secs(3600).await;
    }
}
