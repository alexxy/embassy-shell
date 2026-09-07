//! embassy-shell over USB CDC-ACM on a WeAct "Blackpill" STM32F411CEU6.
//!
//! The shell runs on the USB serial port (PA11 = DM, PA12 = DP, USB OTG FS).
//! Open any terminal program on the "Embassy shell device" COM port, 115200
//! (baud rate is ignored on CDC). The shell respawns every time the host
//! (re)connects the port.
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
use embassy_futures::join::join;
use embassy_futures::select::{select, Either};
use embassy_shell::Shell;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_stm32::rcc::{
    mux::Clk48sel, AHBPrescaler, APBPrescaler, Hse, HseMode, Pll, PllMul, PllPDiv, PllPreDiv,
    PllQDiv, PllSource, Sysclk,
};
use embassy_stm32::time::Hertz;
use embassy_stm32::timer::low_level::{RoundTo, Timer as TickTimer};
use embassy_stm32::usb::{self, Driver};
use embassy_stm32::{bind_interrupts, peripherals};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;
use embassy_usb::class::cdc_acm::{CdcAcmClass, Receiver, Sender, State};
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

const HEAP_SIZE: usize = 16 * 1024;

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
    OTG_FS => usb::InterruptHandler<peripherals::USB_OTG_FS>;
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
    shell.prompt("blackpill-usb> ");
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

#[derive(Debug)]
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
/// A endpoint error (e.g. the host went away) is reported as EOF.
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

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // 25 MHz HSE (Blackpill crystal) -> 96 MHz SYSCLK, PLL1_Q = 48 MHz for USB.
    let mut config = embassy_stm32::Config::default();
    config.rcc.hse = Some(Hse {
        freq: Hertz::mhz(25),
        mode: HseMode::Oscillator,
    });
    config.rcc.pll_src = PllSource::HSE;
    config.rcc.pll = Some(Pll {
        prediv: PllPreDiv::DIV25,
        mul: PllMul::MUL192,
        divp: Some(PllPDiv::DIV2), // 96 MHz
        divq: Some(PllQDiv::DIV4), // 48 MHz
        divr: None,
    });
    config.rcc.sys = Sysclk::PLL1_P;
    config.rcc.ahb_pre = AHBPrescaler::DIV1;
    config.rcc.apb1_pre = APBPrescaler::DIV4;
    config.rcc.apb2_pre = APBPrescaler::DIV2;
    config.rcc.mux.clk48sel = Clk48sel::PLL1_Q;

    let p = embassy_stm32::init(config);
    defmt::info!("embassy-shell on STM32F411 (usb)");

    let heap_ptr = core::ptr::addr_of_mut!(HEAP_MEM);
    unsafe { HEAP.init(heap_ptr as usize, HEAP_SIZE) };

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

    // --- USB CDC-ACM ---
    let mut usb_cfg = usb::Config::default();
    // The Blackpill does not wire VBUS sensing to PA9, so don't check it.
    usb_cfg.vbus_detection = false;

    let mut ep_out_buffer = [0u8; 1024];
    let driver = Driver::new_fs(
        p.USB_OTG_FS,
        Irqs,
        p.PA12,
        p.PA11,
        &mut ep_out_buffer,
        usb_cfg,
    );

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
            // reconnect and spawn a fresh shell.
            let _ = shell.run(&mut reader, &mut writer).await;
        }
    })
    .await;

    loop {
        Timer::after_secs(3600).await;
    }
}
