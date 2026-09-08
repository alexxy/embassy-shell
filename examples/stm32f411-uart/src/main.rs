//! embassy-shell over USART1 on a WeAct "Blackpill" STM32F411CEU6.
//!
//! Console: USART1, PA9 = TX, PA10 = RX, 115200 8N1.
//! Commands: `led on|off|toggle|blink [period_ms]`, `pwm <0-255>|off`,
//! `status`, `reboot`.
//!
//! * onboard LED: PC13 (active low)
//! * PWM on the same PC13 pin: PC13 has no hardware timer channel on the
//!   F411, so the PWM is generated in software with the `spwm` crate, driven
//!   by a 100 kHz TIM3 update interrupt (1 kHz software PWM, 100 ticks per
//!   period). The user-facing brightness range is 0..255; since `spwm`'s
//!   public duty-cycle API is whole percents, an error-feedback dither over
//!   consecutive PWM periods maps all 256 levels onto the 10 us physical
//!   steps.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use core::cell::RefCell;
use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use critical_section::Mutex;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_shell::Shell;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::mode::Async;
use embassy_stm32::timer::low_level::{RoundTo, Timer as TickTimer};
use embassy_stm32::usart::{Config as UartConfig, Uart, UartRx};
use embassy_stm32::{bind_interrupts, peripherals};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;
use embedded_alloc::LlffHeap;
use panic_halt as _;
use spwm::{Spwm, SpwmState};
// Re-exported by embassy-stm32 only as `pub(crate)`; pin the same version it
// depends on (features are unified across the graph by cargo).
use stm32_metapac as pac;

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

/// `spwm` "hardware" tick rate: the TIM3 update interrupt frequency.
const SPWM_TICK_HZ: u32 = 100_000;
/// Software PWM frequency on the LED pin.
const SPWM_PWM_HZ: u32 = 1_000;
/// Physical PWM steps per period (`spwm` tick / PWM frequency).
const PWM_PERIOD_TICKS: u32 = SPWM_TICK_HZ / SPWM_PWM_HZ;

bind_interrupts!(struct Irqs {
    USART1 => embassy_stm32::usart::InterruptHandler<peripherals::USART1>;
    DMA2_STREAM7 => embassy_stm32::dma::InterruptHandler<peripherals::DMA2_CH7>;
    DMA2_STREAM2 => embassy_stm32::dma::InterruptHandler<peripherals::DMA2_CH2>;
});

/// TIM3 update ISR feeding `spwm`. The binding itself is the proof that the
/// interrupt is wired; nothing else needs the unit struct.
struct SpwmTick;

impl embassy_stm32::interrupt::typelevel::Handler<embassy_stm32::interrupt::typelevel::TIM3>
    for SpwmTick
{
    unsafe fn on_interrupt() {
        // Acknowledge the TIM3 update event.
        pac::TIM3.sr().modify(|w| w.set_uif(false));
        critical_section::with(|cs| {
            if let Some(sw) = &*SPWM.borrow_ref(cs) {
                sw.irq_handler();
            }
        });
    }
}

bind_interrupts!(struct SpwmIrqs {
    TIM3 => SpwmTick;
});

/// The `spwm` manager (1 channel). Lives behind a critical-section mutex so
/// both the ISR and the setup code can reach it.
static SPWM: Mutex<RefCell<Option<Spwm<1>>>> = Mutex::new(RefCell::new(None));
/// The onboard LED (active low), driven from `spwm` callbacks only.
static LED_PIN: Mutex<RefCell<Option<Output<'static>>>> = Mutex::new(RefCell::new(None));

/// Current user brightness, 0..=255 (0 = off).
static LED_BRIGHTNESS: AtomicU32 = AtomicU32::new(0);
/// Target "on" time of the PWM period in 1/10_000 of a physical tick.
static BRIGHT_TARGET_X4: AtomicU32 = AtomicU32::new(0);
/// Dithering error accumulator (1/10_000 of a tick), carried between periods.
static DITHER_ERR_X4: AtomicI32 = AtomicI32::new(0);
/// Half-period of `led blink` in milliseconds; 0 = not blinking.
static LED_PERIOD_MS: AtomicU32 = AtomicU32::new(0);

/// Set LED brightness (0..=255). Takes effect from the next PWM period.
fn set_brightness(value: u8) {
    LED_BRIGHTNESS.store(u32::from(value), Ordering::Relaxed);
    BRIGHT_TARGET_X4.store(
        u32::from(value) * PWM_PERIOD_TICKS * 10_000 / 255,
        Ordering::Relaxed,
    );
    // Drop any leftover dither error so the new level is reached promptly.
    DITHER_ERR_X4.store(0, Ordering::Relaxed);
}

/// `spwm` on/off callback: runs in the TIM3 ISR. The LED is active low, so a
/// logical PWM "high" drives the pin low.
fn led_on_off_cb(state: &SpwmState) {
    let on = matches!(state, SpwmState::On);
    critical_section::with(|cs| {
        if let Some(led) = &mut *LED_PIN.borrow_ref_mut(cs) {
            led.set_level(if on { Level::Low } else { Level::High });
        }
    });
}

/// `spwm` period callback: error-feedback dithering. Converts the fractional
/// target "on" time into the whole-percent duty cycle `spwm` accepts and
/// carries the rounding error into the following periods, so the long-run
/// average has the full 10 us tick resolution (0..255 levels).
fn led_period_cb() {
    let desired =
        BRIGHT_TARGET_X4.load(Ordering::Relaxed) as i32 + DITHER_ERR_X4.load(Ordering::Relaxed);
    let duty = ((desired + 5_000) / 10_000).clamp(0, PWM_PERIOD_TICKS as i32) as u8;
    DITHER_ERR_X4.store(desired - i32::from(duty) * 10_000, Ordering::Relaxed);

    critical_section::with(|cs| {
        if let Some(sw) = &*SPWM.borrow_ref(cs) {
            if let Some(ch) = sw.get_channel(0) {
                let _ = ch.update_duty_cycle(duty);
            }
        }
    });
}

#[derive(Clone, Copy)]
enum LedCmd {
    On,
    Off,
    Toggle,
    Blink(u32),
}

static LED_CH: Channel<CriticalSectionRawMutex, LedCmd, 8> = Channel::new();

fn build_shell() -> Shell<'static> {
    let mut shell = Shell::new();
    shell.prompt("blackpill> ");
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
        "pwm <0-255>|off   (PC13 software PWM, 1 kHz)",
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

    shell.add_command("reboot", "reset the board", |_args, mut io| {
        Box::pin(async move {
            io.println("bye!").await?;
            io.flush().await?;
            Timer::after_millis(100).await;
            cortex_m::peripheral::SCB::sys_reset();
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

/// Byte-stream reader on top of the DMA-backed `UartRx`. embassy-stm32 does
/// not implement `embedded-io`'s `Read` for UART, and its DMA read only
/// completes when the whole buffer is full, so we feed it one byte at a time.
struct UartReader<'d>(UartRx<'d, Async>);

impl embedded_io::ErrorType for UartReader<'_> {
    type Error = embassy_stm32::usart::Error;
}

impl embedded_io_async::Read for UartReader<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.0.read(&mut buf[..1]).await?;
        Ok(1)
    }
}

#[embassy_executor::task]
async fn shell_task(uart: Uart<'static, Async>) {
    let (tx, rx) = uart.split();
    let mut reader = UartReader(rx);
    let mut writer = tx;
    loop {
        defmt::info!("shell session");
        let mut shell = build_shell();
        // EOF (e.g. the terminal program closed) ends `run`; just respawn.
        let _ = shell.run(&mut reader, &mut writer).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());
    defmt::info!("embassy-shell on STM32F411 (uart)");

    let heap_ptr = core::ptr::addr_of_mut!(HEAP_MEM);
    unsafe { HEAP.init(heap_ptr as usize, HEAP_SIZE) };

    let mut uart_cfg = UartConfig::default();
    uart_cfg.baudrate = 115_200;
    let uart = Uart::new(
        p.USART1, p.PA10, // RX
        p.PA9,  // TX
        p.DMA2_CH7, p.DMA2_CH2, Irqs, uart_cfg,
    )
    .unwrap();

    // LED starts off (PC13 is active low, so drive it high). All subsequent
    // pin writes go through the `spwm` callbacks.
    critical_section::with(|cs| {
        *LED_PIN.borrow_ref_mut(cs) = Some(Output::new(p.PC13, Level::High, Speed::Low))
    });

    // Software PWM on PC13: 1 kHz channel, 100 steps per period, duty cycle
    // refreshed by the dithering period callback. Stays enabled; brightness
    // 0 simply keeps the "on" time at zero.
    let mut sw = Spwm::<1>::new(SPWM_TICK_HZ);
    let channel = sw
        .create_channel()
        .freq_hz(SPWM_PWM_HZ)
        .duty_cycle(0)
        .on_off_callback(led_on_off_cb)
        .period_callback(led_period_cb)
        .build()
        .unwrap();
    let id = sw.register_channel(channel).unwrap();
    sw.get_channel(id).unwrap().enable().unwrap();
    critical_section::with(|cs| *SPWM.borrow_ref_mut(cs) = Some(sw));

    // TIM3 update interrupt at 100 kHz feeds spwm (embassy-time is on TIM4).
    let tick = TickTimer::new(p.TIM3);
    tick.set_period_us(1_000_000 / SPWM_TICK_HZ, RoundTo::Slower);
    tick.enable_update_interrupt(true);
    tick.start();
    let _spwm_irqs = SpwmIrqs;

    spawner.spawn(led_task().unwrap());

    spawner.spawn(shell_task(uart).unwrap());

    loop {
        Timer::after_secs(3600).await;
    }
}
