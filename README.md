# embassy-shell

A small `no_std` interactive shell in the spirit of bash, built for
[embassy](https://embassy.dev) (any async executor works) and any
transport implementing [`embedded-io-async`](https://docs.rs/embedded-io-async)
— UART, USB CDC, TCP sockets, …

Think [`ushell`](https://github.com/dotcypress/ushell), but alive, native
`async`/embassy, with a simpler command-registration API.

## Features

* **Bash-like line editing** — cursor keys, `Home`/`End`, `Delete`,
  `Backspace`, `Ctrl-U` (kill line), `Ctrl-W` (delete word), `Ctrl-L`
  (clear screen), UTF-8 input.
* **Tab completion** — completes command names; per-command argument
  completion from fixed option lists or custom callbacks. Inserts the common
  prefix and lists candidates when ambiguous, like bash.
* **History** — `Up`/`Down` navigation, configurable size, `history`
  built-in.
* **Ctrl-C interrupts a running command** — the handler future is dropped
  (handlers should be cancel-safe).
* **Simple command registration** — closures returning a boxed future;
  commands get an `Io` output handle so they can `await` slow writes.
* **Built-ins** `help`, `history`, `clear` (can be shadowed by user
  commands).
* Only `embedded-io` + `embedded-io-async` as dependencies. Uses `alloc`
  (a global allocator is required). Not `Send`-bound, which matches
  embassy's per-core executor model.

## Quick start

```rust
use embassy_shell::Shell;

let mut shell = Shell::new();

// A command with free-form arguments.
shell.add_command("echo", "print arguments", |args, mut io| {
    Box::pin(async move { io.println(&args.rest()).await })
});

// A command with a fixed set of first-argument options (`led on|off|blink`).
shell.add_command_with_options("led", "control the led", &["on", "off", "blink"],
    |args, mut io| Box::pin(async move {
        match args.get(0) {
            Some("on") => io.println("led on").await,
            Some("off") => io.println("led off").await,
            _ => io.println("usage: led on|off|blink").await,
        }
    }),
);

// Run over anything that is embedded-io-async Read + Write:
// embassy UART handles, embassy-usb CDC `SerialPort`, ...
shell.run(&mut reader, &mut writer).await?;
```

### With embassy (UART)

```rust
#[embassy_executor::task]
async fn shell_task(mut uart: Uart<'static, PERIPHERALS>) {
    let mut shell = Shell::new();
    // ... add_command(...) ...
    shell.run(&mut uart, &mut uart).await.ok();
}
```

The same works with embassy-usb CDC-ACM: pass the `SerialPort`'s reader and
writer halves.

## Command handlers

A handler is `Fn(Args, Io) -> BoxFuture<'_, Result<()>>`. Wrap an
`async move` block in `Box::pin` (this type-erases per-command future types
so all commands live in one table):

```rust
shell.add_command("wait", "wait n ms, printing dots", |args, mut io| {
    Box::pin(async move {
        for _ in 0..args.get(0).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0) {
            io.print(".").await?;
            Timer::after_millis(100).await;   // any await is fine
        }
        io.println(" done").await
    })
});
```

`Args` gives `get(i)`, `iter()`, `len()` and `rest()` (the arguments joined
with spaces); quotes are handled bash-style (`led "on blink"` → one token).
`Io` provides `print`, `println`, `write_all`, `flush` and implements
`embedded_io_async::Write`, so you can hand it to other libraries.

Custom argument completion:

```rust
shell.add_command_with_completer(
    "set", "set a config key",
    |args, io| Box::pin(async move { /* ... */ io.println("ok").await }),
    |index, prefix| match index {
        1 => KEYS.iter().filter(|k| k.starts_with(prefix)).map(|k| k.to_string()).collect(),
        _ => vec![],
    },
);
```

## Terminal keys

| Key        | Action                                   |
|------------|------------------------------------------|
| `Enter`    | run line                                  |
| `Tab`      | complete command / argument               |
| `Up/Down`  | history                                   |
| `←/→`      | move cursor (ins-line editing supported)  |
| `Home/End` | jump to start / end                       |
| `Backspace` / `Delete` | erase around cursor           |
| `Ctrl-C`   | clear line / **interrupt running command**|
| `Ctrl-U`   | kill line                                 |
| `Ctrl-W`   | delete previous word                      |
| `Ctrl-L`   | clear screen                              |

## Examples

Eight ready-to-flash projects live in [`examples/`](examples/). Each one is a
standalone crate (own `Cargo.toml`, `.cargo/config.toml` and runner), so
`cd` into it and use `cargo check` / `cargo run`.

Every example runs a shell with `led on|off|toggle|blink [period_ms]`,
`pwm <0-255>|off` (the ESP32-C6 examples drive a WS2812 strip instead, via
`rgb <r> <g> <b>|<color>|off`) and `status` (plus `reboot` on the STM32 ones).

| Example | Transport | Console |
|---|---|---|
| [`stm32f411-uart`](examples/stm32f411-uart) | USART1 + DMA | serial, 115200 8N1 |
| [`stm32f411-usb`](examples/stm32f411-usb) | USB CDC-ACM | any COM terminal |
| [`nanoch32v305-uart`](examples/nanoch32v305-uart) | USART1 + DMA | serial, 115200 8N1 |
| [`nanoch32v305-usb`](examples/nanoch32v305-usb) | USB CDC-ACM | any COM terminal |
| [`esp32c6-uart`](examples/esp32c6-uart) | UART0 | serial, 115200 8N1 |
| [`esp32c6-usb`](examples/esp32c6-usb) | USB Serial/JTAG | any COM terminal |
| [`esp32c3-uart`](examples/esp32c3-uart) | UART0 | serial, 115200 8N1 |
| [`esp32c3-usb`](examples/esp32c3-usb) | USB Serial/JTAG | any COM terminal |

All examples log via **defmt** and are flashed with **probe-rs**: `cargo run`
builds, flashes and streams the logs (`DEFMT_LOG=info` is preset in each
`.cargo/config.toml`; override with e.g. `DEFMT_LOG=debug cargo run`).

### STM32F411 Blackpill (STM32F411CE)

* Toolchain: stable, `thumbv7em-none-eabihf`. Flash: `cargo run` (runner is
  `probe-rs run --chip STM32F411CE`), defmt over SWD RTT.
* Console (uart): `PA9` = TX, `PA10` = RX, 115200 8N1.
* USB (usb): built-in OTG FS on the `USB` connector, `PA11` = DM, `PA12` = DP.
  Uses the 25 MHz HSE crystal → 96 MHz core, PLL1_Q = 48 MHz for USB.
* LED: `PC13` (active low). The LED is also the PWM output: `PC13` has no
  hardware timer channel on the F411, so the PWM is generated in software
  with the [`spwm`](https://crates.io/crates/spwm) crate — a 100 kHz TIM3
  interrupt drives a 1 kHz software PWM (100 physical ticks per period), and
  error-feedback dithering maps the whole `0..255` brightness range onto
  10 µs steps. embassy-time runs on TIM4.

### NanoCH32V305 (CH32V305RBT6)

* Toolchain: **nightly** with `rust-src` (custom `riscv32imfc-unknown-none-elf`
  JSON target + `-Zbuild-std`). Flash: `cargo run` (runner is
  `probe-rs run --chip CH32V305RBT6 --connect-under-reset`), defmt over SWD
  RTT.
* Depends on [`ch32-hal`](https://github.com/ch32-rs/ch32-hal) pinned to a
  git revision.
* Console (uart): `PA9` = TX, `PA10` = RX, 115200 8N1.
* USB (usb): OTG FS, `PA11` = DM, `PA12` = DP, core clock 144 MHz from HSI
  (required for the 48 MHz USB clock). These pins are shared with the ISP
  bootloader: if the device never enumerates, make sure the board is not
  being held in the bootloader.
* LED: `PA3` (active low). The LED is also the PWM output: `PA3` = TIM2_CH4
  (no remap), hardware PWM at 1 kHz with active-low polarity, so the user
  brightness `0..255` maps linearly onto the timer duty.

### ESP32-C6 / ESP32-C3

* Toolchain: stable, built-in target `riscv32imac-unknown-none-elf` (C6) /
  `riscv32imc-unknown-none-elf` (C3); esp-hal 1.1 + `esp-rtos` (embassy
  executor). Flash: `cargo run` (runner is `probe-rs run --chip esp32c6` /
  `--chip esp32c3`). The C3 examples share the structure of the C6 ones
  (transport, shell, defmt setup) but drive a plain LED + LEDC PWM instead of
  a WS2812 strip, and differ in chip feature, target and UART0 pins.
* Console (uart): UART0; `GPIO16` = TX, `GPIO17` = RX (C6) or `GPIO21` = TX,
  `GPIO20` = RX (C3), 115200 8N1.
* USB (usb): neither the C6 nor the C3 has **USB-OTG**; these examples use
  the built-in **USB Serial/JTAG** peripheral (the same controller as the
  board's flash port), which enumerates as a CDC device. Because debug and
  USB Serial/JTAG share the connector, the runner adds
  `--connect-under-reset`.
* defmt: on by default in the uart example (RTT over SWD); **off** by default
  in the usb example to avoid contending for the USB Serial/JTAG console —
  enable with `cargo build --features defmt`.
* LED (C6 examples): WS2812 RGB LED on `GPIO8` (`D8` of the ESP32-C6
  Supermini), driven by the RMT peripheral (GRB frames on an 80 MHz tick,
  idle-high between frames). Commands: `rgb <r> <g> <b>` (each 0–255), named
  presets (`red`, `green`, `blue`, `yellow`, `magenta`, `cyan`, `white`) and
  `rgb off`; `led on|off|toggle|blink` shows the last color (white by
  default).
* LED (C3 examples): onboard blue LED on `GPIO8` (active high). The LED is
  also the PWM output: `GPIO8` via LEDC low-speed channel 1, 1 kHz with
  10-bit duty, so the user brightness `0..255` maps linearly onto the timer
  duty.

The shell itself is transport-agnostic: the UART examples plug a small
`embedded-io-async` adapter over the HAL's DMA-backed UART, the USB CDC ones
do the same over embassy-usb endpoints (see `CdcReader`/`CdcWriter`), and the
ESP examples use the HAL's async `Rx`/`Tx` halves directly.

## Notes & limitations

* `alloc` is required (command table, line buffer, history).
* Ctrl-C cancels the handler future: don't hold non-cancel-safe state
  across `.await` inside a command.
* Bytes typed *while* a command runs are not echoed; only the most recent
  one is kept for the next line.
* Completion completes the token at the end of the line.
* ANSI/VT100 terminal assumed (`picocom`, `minicom`, PuTTY, Windows
  Terminal, …); CR/LF/CRLF all accepted as Enter.

## Status / ideas

* [ ] `heapless` back-end so the core works without `alloc`
* [ ] password auth hook, command aliases
* [ ] `Send` variant of the command table for multi-core setups

PRs welcome. MIT OR Apache-2.0 licensed.
