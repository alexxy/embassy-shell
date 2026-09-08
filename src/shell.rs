use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use core::pin::pin;
use core::task::Poll;

use crate::command::{Args, Command, CompleterKind};
use crate::error::Result;
use crate::io::Io;
use crate::keys::{Key, read_key};
use crate::parse::{common_prefix, next_char_boundary, prev_char_boundary, tokenize_into};
use crate::util::{StackWriter, poll_fn};

/// Used to emulate `{:<12}` / `{:>4}` padding in `help`/`history` without
/// allocating via `format!`.
const PADDING: &str = "            ";

/// Build an ANSI cursor-move sequence `ESC [ n dir` without `core::fmt`.
fn cursor_move<const N: usize>(n: usize, dir: u8) -> StackWriter<N> {
    let mut w = StackWriter::<N>::new();
    w.push(0x1b);
    w.push(b'[');
    w.push_usize(n);
    w.push(dir);
    w
}

/// Commands handled by the shell itself. Shown in `help` and completed at the
/// prompt unless a user command shadows one of them.
const BUILTINS: [(&str, &str); 3] = [
    ("help", "list available commands"),
    ("history", "show command history"),
    ("clear", "clear the screen"),
];

/// What happened while a command was running.
#[derive(Debug, PartialEq, Eq)]
enum ExecOutcome {
    /// Command finished, was interrupted, or produced an error.
    Done,
    /// The input reached end-of-file.
    Eof,
}

/// Result of racing a command future against the input stream.
enum Step {
    Done(Result<()>),
    Byte(u8),
    Interrupted,
    Eof,
}

/// The shell.
///
/// The type parameter `'a` is the lifetime of state captured by command
/// handlers registered with [`Shell::add_command`].
///
/// # Example
///
/// ```
/// use embassy_shell::Shell;
///
/// let mut shell = Shell::new();
/// shell.add_command("echo", "print arguments", |args, mut io| {
///     Box::pin(async move { io.println(&args.rest()).await })
/// });
///
/// let mut input: &[u8] = b"echo hello world\r\n";
/// let mut output: Vec<u8> = Vec::new();
/// futures::executor::block_on(shell.run(&mut input, &mut output)).unwrap();
///
/// assert!(String::from_utf8_lossy(&output).contains("hello world"));
/// ```
pub struct Shell<'a> {
    commands: Vec<Command<'a>>,
    history: VecDeque<String>,
    max_history: usize,
    max_history_len: usize,
    max_line_len: usize,
    prompt: &'static str,
}

impl Default for Shell<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Shell<'a> {
    /// Create an empty shell with default settings.
    pub fn new() -> Self {
        Shell {
            commands: Vec::new(),
            history: VecDeque::new(),
            max_history: 32,
            max_history_len: 64,
            max_line_len: 128,
            prompt: "embassy> ",
        }
    }

    /// Set the prompt string.
    pub fn prompt(&mut self, prompt: &'static str) -> &mut Self {
        self.prompt = prompt;
        self
    }

    /// Set the maximum number of remembered history entries.
    pub fn max_history(&mut self, n: usize) -> &mut Self {
        self.max_history = n;
        self
    }

    /// Set the maximum length (in bytes) of a remembered history entry.
    /// Longer lines are truncated (at a character boundary) before being
    /// stored. Defaults to 64; `0` disables history storage of content
    /// beyond the empty string.
    pub fn max_history_len(&mut self, n: usize) -> &mut Self {
        self.max_history_len = n;
        self
    }

    /// Set the maximum length (in bytes) of a line typed at the prompt.
    /// Once the buffer is full, further characters are rejected with a `BEL`
    /// (`\x07`). Defaults to 128.
    pub fn max_line_len(&mut self, n: usize) -> &mut Self {
        self.max_line_len = n;
        self
    }

    /// Register a command.
    ///
    /// `name` is what the user types at the prompt, `help` is the one-line
    /// description shown by `help`. `handler` receives the arguments and an
    /// output handle, and must return a boxed future (wrap an `async move`
    /// block in [`Box::pin`]) so that all commands share one concrete future
    /// type:
    ///
    /// ```
    /// # use embassy_shell::Shell;
    /// let mut shell = Shell::new();
    /// shell.add_command("hello", "say hello", |args, mut io| {
    ///     Box::pin(async move {
    ///         let name = args.get(0).unwrap_or("world");
    ///         io.println(&format!("Hello, {name}!")).await
    ///     })
    /// });
    /// ```
    pub fn add_command<F>(&mut self, name: &str, help: &'static str, handler: F)
    where
        F: for<'x> Fn(Args<'x>, Io<'x>) -> crate::BoxFuture<'x, Result<()>> + 'a,
    {
        self.push_command(Command {
            name: String::from(name),
            help,
            handler: Box::new(handler),
            completer: None,
        });
    }

    /// Register a command whose first argument is completed from a fixed list
    /// of options (e.g. `led on|off|blink`).
    pub fn add_command_with_options<F>(
        &mut self,
        name: &str,
        help: &'static str,
        options: &'static [&'static str],
        handler: F,
    ) where
        F: for<'x> Fn(Args<'x>, Io<'x>) -> crate::BoxFuture<'x, Result<()>> + 'a,
    {
        self.push_command(Command {
            name: String::from(name),
            help,
            handler: Box::new(handler),
            completer: Some(CompleterKind::Options(options)),
        });
    }

    /// Register a command with a custom completer. The completer is called
    /// with the index of the argument being completed (`>= 1`) and the
    /// partial token, and returns candidate replacements for that token.
    pub fn add_command_with_completer<F, C>(
        &mut self,
        name: &str,
        help: &'static str,
        handler: F,
        completer: C,
    ) where
        F: for<'x> Fn(Args<'x>, Io<'x>) -> crate::BoxFuture<'x, Result<()>> + 'a,
        C: Fn(usize, &str) -> Vec<String> + 'a,
    {
        self.push_command(Command {
            name: String::from(name),
            help,
            handler: Box::new(handler),
            completer: Some(CompleterKind::Custom(Box::new(completer))),
        });
    }

    /// Names of all registered user commands.
    pub fn command_names(&self) -> impl Iterator<Item = &str> {
        self.commands.iter().map(|c| c.name.as_str())
    }

    fn push_command(&mut self, cmd: Command<'a>) {
        if let Some(existing) = self.commands.iter_mut().find(|c| c.name == cmd.name) {
            *existing = Command {
                name: cmd.name,
                help: cmd.help,
                handler: cmd.handler,
                completer: cmd.completer,
            };
        } else {
            self.commands.push(cmd);
        }
    }

    /// Run the interactive shell until end of input.
    ///
    /// `reader` and `writer` are the transport (UART, USB CDC, ...). They may
    /// be any types implementing [`embedded_io_async::Read`] /
    /// [`embedded_io_async::Write`].
    ///
    /// Ctrl-C interrupts a running command (the handler future is dropped, so
    /// handlers should be cancel-safe). Bytes typed while a command runs are
    /// not echoed and only the most recent one is kept for the next line.
    pub async fn run<R, W>(&mut self, reader: &mut R, writer: &mut W) -> Result<()>
    where
        R: embedded_io_async::Read,
        W: embedded_io_async::Write,
    {
        let mut pending: Option<u8> = None;
        let mut buf = String::new();
        let mut cursor: usize = 0;
        let mut hist_idx: usize = 0;
        let mut draft = String::new();
        // Token slots are reused across lines: after the longest line ever
        // executed, tokenization stops allocating.
        let mut tokens: Vec<String> = Vec::new();

        Io::new(writer).print(self.prompt).await?;

        loop {
            let Some(key) = read_key(reader, &mut pending).await? else {
                // End of input.
                log!("input eof");
                return Ok(());
            };
            log!("key: {}", key);

            match key {
                Key::Char(c) => {
                    if buf.len() + c.len_utf8() > self.max_line_len {
                        // Line buffer is full: beep and drop the character.
                        Io::new(writer).print("\x07").await?;
                    } else {
                        let at_end = cursor == buf.len();
                        let mut tmp = [0u8; 4];
                        let s = c.encode_utf8(&mut tmp);
                        buf.insert_str(cursor, s);
                        cursor += s.len();
                        if at_end {
                            Io::new(writer).print(s).await?;
                        } else {
                            self.redraw(writer, &buf, cursor).await?;
                        }
                    }
                }
                Key::Enter => {
                    Io::new(writer).print("\r\n").await?;

                    if !buf.trim().is_empty() {
                        log!("line: {}", buf.as_str());
                        self.push_history(&buf);
                        let n = tokenize_into(&buf, &mut tokens);
                        match self
                            .exec(&tokens[..n], reader, &mut pending, writer)
                            .await?
                        {
                            ExecOutcome::Done => {}
                            ExecOutcome::Eof => return Ok(()),
                        }
                    }
                    // Clear (rather than replace) so the buffer's capacity is
                    // reused by the next line.
                    buf.clear();
                    cursor = 0;
                    draft.clear();
                    hist_idx = self.history.len();
                    Io::new(writer).print(self.prompt).await?;
                }
                Key::Backspace => {
                    if cursor > 0 {
                        let start = prev_char_boundary(&buf, cursor);
                        buf.replace_range(start..cursor, "");
                        cursor = start;
                        self.redraw(writer, &buf, cursor).await?;
                    }
                }
                Key::Delete => {
                    if cursor < buf.len() {
                        let end = next_char_boundary(&buf, cursor);
                        buf.replace_range(cursor..end, "");
                        self.redraw(writer, &buf, cursor).await?;
                    }
                }
                Key::Left => {
                    if cursor > 0 {
                        cursor = prev_char_boundary(&buf, cursor);
                        Io::new(writer).print("\x1b[D").await?;
                    }
                }
                Key::Right => {
                    if cursor < buf.len() {
                        cursor = next_char_boundary(&buf, cursor);
                        Io::new(writer).print("\x1b[C").await?;
                    }
                }
                Key::Home => {
                    if cursor > 0 {
                        let n = buf[..cursor].chars().count();
                        Io::new(writer)
                            .write_all(cursor_move::<16>(n, b'D').as_bytes())
                            .await?;
                        cursor = 0;
                    }
                }
                Key::End => {
                    if cursor < buf.len() {
                        let n = buf[cursor..].chars().count();
                        Io::new(writer)
                            .write_all(cursor_move::<16>(n, b'C').as_bytes())
                            .await?;
                        cursor = buf.len();
                    }
                }
                Key::Tab => {
                    self.complete(writer, &mut buf, &mut cursor).await?;
                }
                Key::Up => {
                    if !self.history.is_empty() {
                        if hist_idx == self.history.len() {
                            draft = buf.clone();
                        }
                        if hist_idx > 0 {
                            hist_idx -= 1;
                            buf = self.history[hist_idx].clone();
                            cursor = buf.len();
                            self.redraw(writer, &buf, cursor).await?;
                        }
                    }
                }
                Key::Down => {
                    if hist_idx < self.history.len() {
                        hist_idx += 1;
                        buf = if hist_idx == self.history.len() {
                            draft.clone()
                        } else {
                            self.history[hist_idx].clone()
                        };
                        cursor = buf.len();
                        self.redraw(writer, &buf, cursor).await?;
                    }
                }
                Key::CtrlC => {
                    buf.clear();
                    cursor = 0;
                    hist_idx = self.history.len();
                    Io::new(writer).print("^C\r\n").await?;
                    Io::new(writer).print(self.prompt).await?;
                }
                Key::CtrlU => {
                    buf.clear();
                    cursor = 0;
                    self.redraw(writer, &buf, cursor).await?;
                }
                Key::CtrlW => {
                    let end = buf.trim_end().len();
                    let start = buf[..end]
                        .rfind(char::is_whitespace)
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    if start < end {
                        buf.replace_range(start..end, "");
                        cursor = start;
                        self.redraw(writer, &buf, cursor).await?;
                    }
                }
                Key::CtrlL => {
                    Io::new(writer).print("\x1b[2J\x1b[H").await?;
                    self.redraw(writer, &buf, cursor).await?;
                }
            }
        }
    }

    fn push_history(&mut self, line: &str) {
        // Store at most `max_history_len` bytes, cut at a char boundary.
        let mut end = line.len().min(self.max_history_len);
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        let line = &line[..end];
        if self.history.back().map(|h| h == line).unwrap_or(false) {
            return;
        }
        self.history.push_back(String::from(line));
        while self.history.len() > self.max_history {
            self.history.pop_front();
        }
    }

    /// Rewrite the current input line and place the cursor back at `cursor`.
    async fn redraw<W>(&self, writer: &mut W, buf: &str, cursor: usize) -> Result<()>
    where
        W: embedded_io_async::Write,
    {
        // Rendered as several small writes instead of one allocated string.
        let mut io = Io::new(writer);
        io.print("\r").await?;
        io.print(self.prompt).await?;
        io.print(buf).await?;
        // Erase any leftovers from the previous render.
        io.print("\x1b[K").await?;
        let tail = buf[cursor..].chars().count();
        if tail > 0 {
            io.write_all(cursor_move::<16>(tail, b'D').as_bytes())
                .await?;
        }
        Ok(())
    }

    /// Tab completion for the token under the cursor (which must be at the
    /// end of the input line, like most simple terminals).
    async fn complete<W>(&self, writer: &mut W, buf: &mut String, cursor: &mut usize) -> Result<()>
    where
        W: embedded_io_async::Write,
    {
        // Only complete the token at the very end of the line.
        if *cursor != buf.len() {
            return Ok(());
        }

        let ends_with_ws = buf.is_empty() || buf.ends_with(|c: char| c.is_whitespace());
        let n_tokens = buf.split_whitespace().count();
        let (prefix, tok_index) = if ends_with_ws {
            ("", n_tokens)
        } else {
            (
                buf.split_whitespace().last().unwrap_or(""),
                n_tokens.saturating_sub(1),
            )
        };
        let prefix_len = prefix.len();

        // Candidates are borrowed where possible; custom completers still
        // return owned Strings, kept alive by `owned`.
        let owned: Vec<String>;
        let candidates: Vec<&str> = if tok_index == 0 {
            let mut cands: Vec<&str> = self
                .commands
                .iter()
                .filter(|c| c.name.starts_with(prefix))
                .map(|c| c.name.as_str())
                .collect();
            for (name, _) in BUILTINS {
                if name.starts_with(prefix) && !self.commands.iter().any(|c| c.name == name) {
                    cands.push(name);
                }
            }
            cands
        } else {
            let first = buf.split_whitespace().next().unwrap_or("");
            match self
                .commands
                .iter()
                .find(|c| c.name == first)
                .and_then(|c| c.completer.as_ref())
            {
                Some(CompleterKind::Options(options)) => options
                    .iter()
                    .filter(|o| o.starts_with(prefix))
                    .copied()
                    .collect(),
                Some(CompleterKind::Custom(completer)) => {
                    owned = completer(tok_index, prefix);
                    owned.iter().map(String::as_str).collect()
                }
                None => Vec::new(),
            }
        };

        log!("completion: {} candidates for {}", candidates.len(), prefix);
        if candidates.is_empty() {
            return Ok(());
        }

        if candidates.len() == 1 {
            buf.truncate(buf.len() - prefix_len);
            buf.push_str(candidates[0]);
            // Bash appends a space after a unique completion.
            buf.push(' ');
            *cursor = buf.len();
            return self.redraw(writer, buf, *cursor).await;
        }

        // Multiple candidates: insert the common prefix, then list them all.
        let cp = common_prefix(&candidates);
        if cp.len() > prefix_len {
            buf.truncate(buf.len() - prefix_len);
            buf.push_str(cp);
            *cursor = buf.len();
        }

        let mut io = Io::new(writer);
        io.print("\r\n").await?;
        for (i, &cand) in candidates.iter().enumerate() {
            if i > 0 {
                io.print("  ").await?;
            }
            io.print(cand).await?;
        }
        io.print("\r\n").await?;
        self.redraw(writer, buf, *cursor).await
    }

    async fn exec<R, W>(
        &self,
        tokens: &[String],
        reader: &mut R,
        pending: &mut Option<u8>,
        writer: &mut W,
    ) -> Result<ExecOutcome>
    where
        R: embedded_io_async::Read,
        W: embedded_io_async::Write,
    {
        let name = tokens.first().map(String::as_str).unwrap_or("");

        // User commands win over builtins (bash-style overridability).
        if let Some(cmd) = self.commands.iter().find(|c| c.name == name) {
            return self.run_cmd(cmd, tokens, reader, pending, writer).await;
        }

        let mut io = Io::new(writer);
        match name {
            "help" => {
                for cmd in &self.commands {
                    let pad = &PADDING[..12usize.saturating_sub(cmd.name.chars().count())];
                    io.print("  ").await?;
                    io.print(&cmd.name).await?;
                    io.print(pad).await?;
                    io.println(cmd.help).await?;
                }
                for (bname, bhelp) in BUILTINS {
                    if !self.commands.iter().any(|c| c.name == bname) {
                        let pad = &PADDING[..12usize.saturating_sub(bname.chars().count())];
                        io.print("  ").await?;
                        io.print(bname).await?;
                        io.print(pad).await?;
                        io.println(bhelp).await?;
                    }
                }
            }
            "clear" => {
                io.print("\x1b[2J\x1b[H").await?;
            }
            "history" => {
                for (i, entry) in self.history.iter().enumerate() {
                    let mut w = StackWriter::<8>::new();
                    w.push_usize(i + 1);
                    let num = w.as_bytes();
                    io.print(&PADDING[..4usize.saturating_sub(num.len())])
                        .await?;
                    io.write_all(num).await?;
                    io.print("  ").await?;
                    io.println(entry).await?;
                }
            }
            _ => {
                log!("unknown command: {}", name);
                io.print(name).await?;
                io.println(": command not found").await?;
            }
        }
        Ok(ExecOutcome::Done)
    }

    /// Run one command, racing it against the input so Ctrl-C can cancel it.
    async fn run_cmd<R, W>(
        &self,
        cmd: &Command<'a>,
        tokens: &[String],
        reader: &mut R,
        pending: &mut Option<u8>,
        writer: &mut W,
    ) -> Result<ExecOutcome>
    where
        R: embedded_io_async::Read,
        W: embedded_io_async::Write,
    {
        let outcome = {
            let io = Io::new(&mut *writer);
            let fut = (cmd.handler)(Args::new(&tokens[1..]), io);
            let mut fut = pin!(fut);

            loop {
                let byte_f = async {
                    let mut b = [0u8; 1];
                    match reader.read(&mut b).await {
                        Ok(0) => Step::Eof,
                        Ok(_) => Step::Byte(b[0]),
                        Err(_) => Step::Eof,
                    }
                };
                let mut byte_f = pin!(byte_f);

                let step = poll_fn(|cx| {
                    if let Poll::Ready(r) = fut.as_mut().poll(cx) {
                        return Poll::Ready(Step::Done(r));
                    }
                    byte_f.as_mut().poll(cx)
                })
                .await;

                match step {
                    Step::Byte(3) => break Step::Interrupted,
                    Step::Byte(b) => *pending = Some(b),
                    other => break other,
                }
            }
        };

        match outcome {
            Step::Done(Ok(())) => {}
            Step::Done(Err(e)) => {
                let mut io = Io::new(writer);
                io.print(&cmd.name).await?;
                io.print(": ").await?;
                io.println(e.message()).await?;
            }
            Step::Interrupted => {
                log!("{}: interrupted", cmd.name.as_str());
                Io::new(writer).print("^C\r\n").await?;
            }
            Step::Eof => return Ok(ExecOutcome::Eof),
            Step::Byte(_) => unreachable!(),
        }
        Ok(ExecOutcome::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct NoError;
    impl core::fmt::Display for NoError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "NoError")
        }
    }
    impl core::error::Error for NoError {}
    impl embedded_io::Error for NoError {
        fn kind(&self) -> embedded_io::ErrorKind {
            embedded_io::ErrorKind::Other
        }
    }

    struct SliceReader<'b> {
        data: &'b [u8],
        pos: usize,
    }
    impl embedded_io::ErrorType for SliceReader<'_> {
        type Error = NoError;
    }
    impl embedded_io_async::Read for SliceReader<'_> {
        async fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, NoError> {
            let n = buf.len().min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[derive(Default)]
    struct VecWriter {
        out: Vec<u8>,
    }
    impl embedded_io::ErrorType for VecWriter {
        type Error = NoError;
    }
    impl embedded_io_async::Write for VecWriter {
        async fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, NoError> {
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }
        async fn flush(&mut self) -> core::result::Result<(), NoError> {
            Ok(())
        }
    }

    fn run(shell: &mut Shell<'_>, input: &[u8]) -> String {
        let mut r = SliceReader {
            data: input,
            pos: 0,
        };
        let mut w = VecWriter::default();
        futures::executor::block_on(shell.run(&mut r, &mut w)).unwrap();
        String::from_utf8_lossy(&w.out).into_owned()
    }

    fn echo_cmd(shell: &mut Shell<'_>) {
        shell.add_command("echo", "print arguments", |args, mut io| {
            Box::pin(async move { io.println(&args.rest()).await })
        });
    }

    #[test]
    fn runs_command() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        let out = run(&mut sh, b"echo hello world\r\n");
        assert!(out.contains("hello world"), "{out}");
    }

    #[test]
    fn crlf_and_quoted_args() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        let out = run(&mut sh, b"echo \"one two\"\n");
        assert!(out.contains("one two"), "{out}");
    }

    #[test]
    fn unknown_command() {
        let mut sh = Shell::new();
        let out = run(&mut sh, b"foo bar\r\n");
        assert!(out.contains("foo: command not found"), "{out}");
    }

    #[test]
    fn tab_completes_command_uniquely() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        let out = run(&mut sh, b"ec\t\r\n");
        // The line was completed to `echo ` and then executed.
        assert!(out.contains("embassy> echo "), "{out}");
        assert!(!out.contains("command not found"), "{out}");
    }

    #[test]
    fn tab_lists_ambiguous_candidates() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        sh.add_command("edd", "", |_a, _io| Box::pin(core::future::ready(Ok(()))));
        let out = run(&mut sh, b"e\t");
        assert!(out.contains("echo"), "{out}");
        assert!(out.contains("edd"), "{out}");
        assert!(out.contains("embassy> e"), "{out}");
    }

    #[test]
    fn tab_completes_options() {
        let mut sh = Shell::new();
        sh.add_command_with_options(
            "led",
            "led control",
            &["on", "off", "blink"],
            |args, mut io| Box::pin(async move { io.println(args.get(0).unwrap_or("?")).await }),
        );
        // `of` is a unique prefix of `off`.
        let out = run(&mut sh, b"led of\t\r\n");
        assert!(out.contains("embassy> led off "), "{out}");
        // Executed with `off`.
        assert!(out.contains("\r\noff\r\n"), "{out}");
        // `o` is ambiguous between `on` and `off`: both are listed.
        let out = run(&mut sh, b"led o\t");
        assert!(out.contains("on") && out.contains("off"), "{out}");
        assert!(out.contains("embassy> led o"), "{out}");
    }

    #[test]
    fn history_recall() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        let out = run(&mut sh, b"echo one\r\necho two\r\n\x1b[A\r\n");
        // `two` printed twice (second run via history), `one` once.
        assert_eq!(out.matches("\r\ntwo\r\n").count(), 2, "{out}");
        assert_eq!(out.matches("\r\none\r\n").count(), 1, "{out}");
    }

    #[test]
    fn history_builtin_lists() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        let out = run(&mut sh, b"echo x\r\nhistory\r\n");
        assert!(out.contains("echo x"), "{out}");
    }

    #[test]
    fn ctrl_c_at_prompt_clears_line() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        let out = run(&mut sh, b"garbage\x03echo ok\r\n");
        // After Ctrl-C the line is discarded; the next line still runs.
        assert!(out.contains("ok"), "{out}");
        assert!(!out.contains("garbage: command not found"), "{out}");
    }

    #[test]
    fn ctrl_c_interrupts_running_command() {
        let mut sh = Shell::new();
        sh.add_command("hang", "never returns", |_args, _io| {
            Box::pin(core::future::pending::<crate::Result<()>>())
        });
        let out = run(&mut sh, b"hang\r\x03");
        assert!(out.contains("^C"), "{out}");
    }

    #[test]
    fn help_lists_commands_and_builtins() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        let out = run(&mut sh, b"help\r\n");
        assert!(out.contains("echo"), "{out}");
        assert!(out.contains("list available commands"), "{out}");
        assert!(out.contains("history"), "{out}");
    }

    #[test]
    fn ctrl_u_kills_line() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        let out = run(&mut sh, b"oops\x15echo fine\r\n");
        assert!(out.contains("fine"), "{out}");
        assert!(!out.contains("oops: command not found"), "{out}");
    }

    #[test]
    fn left_right_editing() {
        let mut sh = Shell::new();
        echo_cmd(&mut sh);
        // Type "echo o", Home, then insert "ec" is fiddly; instead:
        // "ech o" -> Left, Right, Backspace x? Simpler: type "echX", Left,
        // Backspace, "o" => "echo", Enter with arg... we type "ech o" then
        // Left, Backspace => "echo ", Enter (no args) prints empty line.
        let out = run(&mut sh, b"ech o\x1b[D\x7f\r\n");
        assert!(!out.contains("command not found"), "{out}");
    }

    #[test]
    fn line_cap_rejects_with_bell() {
        let mut sh = Shell::new();
        sh.max_line_len(5);
        echo_cmd(&mut sh);
        // "echo " fills the 5-byte cap; the argument is rejected with BELs
        // and never reaches the command.
        let out = run(&mut sh, b"echo hello\r\n");
        assert!(out.contains('\u{7}'), "{out:?}");
        assert!(!out.contains("hello"), "{out:?}");
        assert!(!out.contains("command not found"), "{out:?}");
    }

    #[test]
    fn history_entry_truncated() {
        let mut sh = Shell::new();
        sh.max_history_len(4);
        // `help` is a builtin: its own output cannot be confused with the
        // history listing below.
        let out = run(&mut sh, b"help XXXXXXXXXXXX\nhistory\r\n");
        // The listing shows the entry truncated to 4 bytes.
        assert!(out.contains("   1  help\r\n"), "{out:?}");
        assert!(!out.contains("   1  help "), "{out:?}");
    }
}
