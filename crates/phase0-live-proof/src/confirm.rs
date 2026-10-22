//! Interactive one-time confirmation before a live run.
//!
//! The harness enrolls real FCM and Facepunch identities, so a human must be
//! at an interactive terminal and type back a fresh random code that is shown
//! only there. The code is never logged, recorded, or part of any `Debug`
//! output, and operator input is never echoed back.
//!
//! All terminal access goes through [`ConfirmationIo`] so every refusal path
//! is testable without a real terminal.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::AsFd,
    time::{Duration, Instant},
};

use rand::seq::IndexedRandom;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::helpers::discard_pending_input;

/// Uppercase letters and digits with no `0`, `O`, `1`, or `I`.
pub const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 8;
/// How long the operator has to type the code back.
pub const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Longest line read. The code is [`CODE_LEN`] characters, so anything longer
/// cannot match and is refused as unterminated.
const MAX_LINE_BYTES: usize = 64;
const PROMPT_PREFIX: &str = "Type this one-time code to start the live run \
(real FCM enrollment and Facepunch registration will follow): ";

/// Why a line could not be read. Fixed variants so no input is ever carried.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineFailure {
    EndOfInput,
    Unterminated,
    TimedOut,
    Unreadable,
}

/// Why confirmation was refused. Every variant maps to one fixed message and
/// exit code; none carries operator input or the code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ConfirmRefusal {
    #[error("stdin and stdout are not both interactive terminals")]
    NotATerminal,
    #[error("the confirmation prompt could not be shown")]
    PromptUnwritable,
    #[error("no complete line was read: {0:?}")]
    Input(LineFailure),
    #[error("the typed code did not match")]
    Mismatch,
    #[error("interrupted while waiting for confirmation")]
    Interrupted,
    #[error("confirmation could not run")]
    Internal,
}

/// Terminal access used by [`confirm`].
pub trait ConfirmationIo {
    /// Whether both standard input and standard output are terminals, so the
    /// code is shown to, and typed by, someone at that terminal and never
    /// lands in a file or pipe.
    fn is_interactive_terminal(&self) -> bool;
    /// A fresh one-time code for this run.
    fn new_code(&mut self) -> Zeroizing<String>;
    /// Shows `text` to the operator, discarding any input already waiting.
    fn show_prompt(&mut self, text: &str) -> io::Result<()>;
    /// Reads one line including its terminating `\n`, giving up after
    /// `timeout`. A line without a trailing `\n` is a failure.
    fn read_line(&mut self, timeout: Duration) -> Result<Zeroizing<String>, LineFailure>;
}

/// Generates a code from a CSPRNG (`rand::rng()` is ChaCha reseeded from the
/// operating system).
#[must_use]
pub fn generate_code() -> Zeroizing<String> {
    let mut rng = rand::rng();
    Zeroizing::new(
        (0..CODE_LEN)
            .filter_map(|_| CODE_ALPHABET.choose(&mut rng).copied().map(char::from))
            .collect(),
    )
}

/// Runs the whole confirmation: terminal check, fresh code, prompt, one
/// bounded line, exact comparison.
pub fn confirm(io: &mut impl ConfirmationIo) -> Result<(), ConfirmRefusal> {
    if !io.is_interactive_terminal() {
        return Err(ConfirmRefusal::NotATerminal);
    }
    let code = io.new_code();
    let prompt = Zeroizing::new(format!("{PROMPT_PREFIX}{}", code.as_str()));
    io.show_prompt(&prompt)
        .map_err(|_| ConfirmRefusal::PromptUnwritable)?;
    let line = io
        .read_line(CONFIRMATION_TIMEOUT)
        .map_err(ConfirmRefusal::Input)?;
    let typed = line
        .strip_suffix('\n')
        .ok_or(ConfirmRefusal::Input(LineFailure::Unterminated))?;
    if typed == code.as_str() {
        Ok(())
    } else {
        Err(ConfirmRefusal::Mismatch)
    }
}

/// Runs [`confirm`] on a blocking thread so operator interrupt ends the wait.
/// An interrupted read thread is abandoned; the caller's runtime shutdown
/// timeout keeps it from holding the process open.
pub async fn confirm_interruptible<C>(
    mut io: C,
    cancel: &CancellationToken,
) -> Result<(), ConfirmRefusal>
where
    C: ConfirmationIo + Send + 'static,
{
    let task = tokio::task::spawn_blocking(move || confirm(&mut io));
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ConfirmRefusal::Interrupted),
        joined = task => joined.unwrap_or(Err(ConfirmRefusal::Internal)),
    }
}

/// The real terminal: duplicates of the stdin and stdout descriptors, so
/// std's shared buffered handles can never supply, swallow, or hold bytes.
pub struct StdTerminal {
    input: Option<File>,
    output: Option<File>,
}

impl StdTerminal {
    #[must_use]
    pub fn new() -> Self {
        Self {
            input: rustix::io::dup(rustix::stdio::stdin()).ok().map(File::from),
            output: rustix::io::dup(rustix::stdio::stdout())
                .ok()
                .map(File::from),
        }
    }
}

/// The confirmation is interactive only when the code is shown on a terminal
/// and typed from a terminal: a redirected stdout would write the code into a
/// file or pipe.
#[must_use]
fn both_are_terminals(input: Option<&File>, output: Option<&File>) -> bool {
    input.zip(output).is_some_and(|(input, output)| {
        rustix::termios::isatty(input) && rustix::termios::isatty(output)
    })
}

impl ConfirmationIo for StdTerminal {
    fn is_interactive_terminal(&self) -> bool {
        both_are_terminals(self.input.as_ref(), self.output.as_ref())
    }

    fn new_code(&mut self) -> Zeroizing<String> {
        generate_code()
    }

    fn show_prompt(&mut self, text: &str) -> io::Result<()> {
        if let Some(input) = self.input.as_mut() {
            discard_pending_input(input);
        }
        let out = self
            .output
            .as_mut()
            .ok_or_else(|| io::Error::other("stdout unavailable"))?;
        writeln!(out, "{text}")?;
        out.flush()
    }

    fn read_line(&mut self, timeout: Duration) -> Result<Zeroizing<String>, LineFailure> {
        let input = self.input.as_mut().ok_or(LineFailure::Unreadable)?;
        read_line_bounded(input, timeout)
    }
}

/// Reads up to [`MAX_LINE_BYTES`] and returns the first newline-terminated
/// line (newline included). Polls with the remaining time before every read
/// so a silent terminal ends at `timeout` instead of blocking forever.
fn read_line_bounded(
    input: &mut (impl Read + AsFd),
    timeout: Duration,
) -> Result<Zeroizing<String>, LineFailure> {
    let deadline = Instant::now() + timeout;
    let mut buf = Zeroizing::new([0u8; MAX_LINE_BYTES]);
    let mut filled = 0;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(LineFailure::TimedOut);
        }
        let limit = Timespec {
            tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(remaining.subsec_nanos()),
        };
        let mut fds = [PollFd::new(&*input, PollFlags::IN)];
        match poll(&mut fds, Some(&limit)) {
            Ok(0) => return Err(LineFailure::TimedOut),
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(_) => return Err(LineFailure::Unreadable),
        }
        let window = buf.get_mut(filled..).ok_or(LineFailure::Unterminated)?;
        if window.is_empty() {
            return Err(LineFailure::Unterminated);
        }
        match input.read(window) {
            Ok(0) if filled == 0 => return Err(LineFailure::EndOfInput),
            Ok(0) => return Err(LineFailure::Unterminated),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(LineFailure::Unreadable),
        }
        let received = buf.get(..filled).ok_or(LineFailure::Unreadable)?;
        if let Some(end) = received.iter().position(|byte| *byte == b'\n') {
            let line = received.get(..=end).ok_or(LineFailure::Unreadable)?;
            return std::str::from_utf8(line)
                .map(|text| Zeroizing::new(text.to_owned()))
                .map_err(|_| LineFailure::Unreadable);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{collections::VecDeque, os::unix::net::UnixStream};

    use super::*;

    struct Fake {
        terminal: bool,
        code: &'static str,
        script: VecDeque<Result<&'static str, LineFailure>>,
        shown: Vec<String>,
        timeouts: Vec<Duration>,
        prompt_error: bool,
    }

    impl Fake {
        fn typing(terminal: bool, reply: Result<&'static str, LineFailure>) -> Self {
            Self {
                terminal,
                code: "ABCD2345",
                script: VecDeque::from([reply]),
                shown: Vec::new(),
                timeouts: Vec::new(),
                prompt_error: false,
            }
        }
    }

    impl ConfirmationIo for Fake {
        fn is_interactive_terminal(&self) -> bool {
            self.terminal
        }

        fn new_code(&mut self) -> Zeroizing<String> {
            Zeroizing::new(self.code.to_owned())
        }

        fn show_prompt(&mut self, text: &str) -> io::Result<()> {
            if self.prompt_error {
                return Err(io::Error::other("closed"));
            }
            self.shown.push(text.to_owned());
            Ok(())
        }

        fn read_line(&mut self, timeout: Duration) -> Result<Zeroizing<String>, LineFailure> {
            self.timeouts.push(timeout);
            self.script
                .pop_front()
                .unwrap_or(Err(LineFailure::EndOfInput))
                .map(|line| Zeroizing::new(line.to_owned()))
        }
    }

    #[test]
    fn non_terminal_is_refused_without_prompting_or_reading() {
        let mut io = Fake::typing(false, Ok("ABCD2345\n"));
        assert_eq!(confirm(&mut io), Err(ConfirmRefusal::NotATerminal));
        assert!(io.shown.is_empty());
        assert!(io.timeouts.is_empty());
    }

    #[test]
    fn correct_code_is_accepted_after_the_fixed_prompt() {
        let mut io = Fake::typing(true, Ok("ABCD2345\n"));
        assert_eq!(confirm(&mut io), Ok(()));
        assert_eq!(
            io.shown,
            [
                "Type this one-time code to start the live run (real FCM enrollment and \
Facepunch registration will follow): ABCD2345"
            ]
        );
        assert_eq!(io.timeouts, [Duration::from_secs(300)]);
    }

    #[test]
    fn wrong_code_is_refused_exactly() {
        for reply in [
            "WRONG234\n",
            "abcd2345\n",
            " ABCD2345\n",
            "ABCD2345 \n",
            "ABCD2345\r\n",
            "ABCD234\n",
            "ABCD23456\n",
            "\n",
        ] {
            let mut io = Fake::typing(true, Ok(reply));
            assert_eq!(confirm(&mut io), Err(ConfirmRefusal::Mismatch), "{reply:?}");
        }
    }

    #[test]
    fn end_of_input_timeout_and_read_errors_are_refused() {
        for failure in [
            LineFailure::EndOfInput,
            LineFailure::Unterminated,
            LineFailure::TimedOut,
            LineFailure::Unreadable,
        ] {
            let mut io = Fake::typing(true, Err(failure));
            assert_eq!(confirm(&mut io), Err(ConfirmRefusal::Input(failure)));
        }
    }

    #[test]
    fn unterminated_line_is_refused_even_when_it_matches() {
        let mut io = Fake::typing(true, Ok("ABCD2345"));
        assert_eq!(
            confirm(&mut io),
            Err(ConfirmRefusal::Input(LineFailure::Unterminated))
        );
    }

    #[test]
    fn unwritable_prompt_is_refused_without_reading() {
        let mut io = Fake::typing(true, Ok("ABCD2345\n"));
        io.prompt_error = true;
        assert_eq!(confirm(&mut io), Err(ConfirmRefusal::PromptUnwritable));
        assert!(io.timeouts.is_empty());
    }

    #[test]
    fn refusal_text_never_carries_the_code_or_input() {
        let mut io = Fake::typing(true, Ok("hunter2xx\n"));
        let refusal = confirm(&mut io).expect_err("mismatch");
        for text in [refusal.to_string(), format!("{refusal:?}")] {
            assert!(!text.contains("hunter2"));
            assert!(!text.contains("ABCD2345"));
        }
    }

    #[tokio::test]
    async fn interrupt_ends_the_wait() {
        struct Blocked(std::sync::mpsc::Receiver<()>);
        impl ConfirmationIo for Blocked {
            fn is_interactive_terminal(&self) -> bool {
                true
            }
            fn new_code(&mut self) -> Zeroizing<String> {
                Zeroizing::new("ABCD2345".to_owned())
            }
            fn show_prompt(&mut self, _: &str) -> io::Result<()> {
                Ok(())
            }
            fn read_line(&mut self, _: Duration) -> Result<Zeroizing<String>, LineFailure> {
                let _ = self.0.recv();
                Err(LineFailure::EndOfInput)
            }
        }
        let (release, gate) = std::sync::mpsc::channel();
        let cancel = CancellationToken::new();
        let interrupter = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            interrupter.cancel();
        });
        assert_eq!(
            confirm_interruptible(Blocked(gate), &cancel).await,
            Err(ConfirmRefusal::Interrupted)
        );
        drop(release);
    }

    #[tokio::test]
    async fn interruptible_confirmation_passes_the_result_through() {
        let cancel = CancellationToken::new();
        assert_eq!(
            confirm_interruptible(Fake::typing(true, Ok("ABCD2345\n")), &cancel).await,
            Ok(())
        );
        assert_eq!(
            confirm_interruptible(Fake::typing(false, Ok("ABCD2345\n")), &cancel).await,
            Err(ConfirmRefusal::NotATerminal)
        );
    }

    #[test]
    fn generated_codes_use_the_unambiguous_alphabet() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let code = generate_code();
            assert_eq!(code.len(), 8);
            assert!(
                code.bytes().all(|byte| CODE_ALPHABET.contains(&byte)),
                "{}",
                code.as_str()
            );
            assert!(!code.contains(['0', 'O', '1', 'I']));
            seen.insert(code.as_str().to_owned());
        }
        assert!(seen.len() > 1, "codes must differ between runs");
    }

    #[test]
    fn alphabet_is_uppercase_alphanumeric_without_lookalikes() {
        assert!(
            CODE_ALPHABET
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        );
        for banned in *b"0O1I" {
            assert!(!CODE_ALPHABET.contains(&banned));
        }
        let unique: std::collections::HashSet<_> = CODE_ALPHABET.iter().collect();
        assert_eq!(unique.len(), CODE_ALPHABET.len());
    }

    // --- real descriptor reader, driven through a socket pair ---

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socketpair")
    }

    #[test]
    fn reader_returns_the_first_complete_line() {
        let (mut reader, mut writer) = pair();
        writer.write_all(b"ABCD2345\nextra").expect("write");
        let line = read_line_bounded(&mut reader, Duration::from_secs(5)).expect("line");
        assert_eq!(line.as_str(), "ABCD2345\n");
    }

    #[test]
    fn reader_assembles_a_line_across_reads() {
        let (mut reader, mut writer) = pair();
        writer.write_all(b"ABCD").expect("write");
        let finisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            writer.write_all(b"2345\n").expect("write");
            writer
        });
        let line = read_line_bounded(&mut reader, Duration::from_secs(5)).expect("line");
        assert_eq!(line.as_str(), "ABCD2345\n");
        drop(finisher.join().expect("join"));
    }

    #[test]
    fn reader_times_out_on_a_silent_terminal() {
        let (mut reader, _writer) = pair();
        let started = Instant::now();
        assert_eq!(
            read_line_bounded(&mut reader, Duration::from_millis(150)),
            Err(LineFailure::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn reader_reports_end_of_input_and_unterminated_input() {
        let (mut reader, writer) = pair();
        drop(writer);
        assert_eq!(
            read_line_bounded(&mut reader, Duration::from_secs(5)),
            Err(LineFailure::EndOfInput)
        );
        let (mut reader, mut writer) = pair();
        writer.write_all(b"ABCD2345").expect("write");
        drop(writer);
        assert_eq!(
            read_line_bounded(&mut reader, Duration::from_secs(5)),
            Err(LineFailure::Unterminated)
        );
    }

    #[test]
    fn reader_refuses_an_overlong_line_and_invalid_utf8() {
        let (mut reader, mut writer) = pair();
        writer.write_all(&[b'A'; 200]).expect("write");
        assert_eq!(
            read_line_bounded(&mut reader, Duration::from_secs(5)),
            Err(LineFailure::Unterminated)
        );
        let (mut reader, mut writer) = pair();
        writer.write_all(&[0xff, 0xfe, b'\n']).expect("write");
        assert_eq!(
            read_line_bounded(&mut reader, Duration::from_secs(5)),
            Err(LineFailure::Unreadable)
        );
    }

    fn socket_file() -> (File, UnixStream) {
        let (ours, theirs) = pair();
        (File::from(std::os::fd::OwnedFd::from(ours)), theirs)
    }

    /// A real terminal descriptor: the master side of a fresh pseudo-terminal.
    fn tty_file() -> File {
        let master =
            rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
                .expect("openpt");
        File::from(master)
    }

    #[test]
    fn only_a_terminal_on_both_ends_is_interactive() {
        let (socket_a, _keep_a) = socket_file();
        let (socket_b, _keep_b) = socket_file();
        let (tty_in, tty_out) = (tty_file(), tty_file());
        assert!(
            rustix::termios::isatty(&tty_in),
            "pty master must report as a terminal"
        );
        assert!(both_are_terminals(Some(&tty_in), Some(&tty_out)));
        // stdin is a terminal but stdout is redirected: the code would leak
        // into a file or pipe.
        assert!(!both_are_terminals(Some(&tty_in), Some(&socket_a)));
        // stdout is a terminal but stdin is not.
        assert!(!both_are_terminals(Some(&socket_a), Some(&tty_out)));
        assert!(!both_are_terminals(Some(&socket_a), Some(&socket_b)));
        assert!(!both_are_terminals(Some(&tty_in), None));
        assert!(!both_are_terminals(None, Some(&tty_out)));
        assert!(!both_are_terminals(None, None));
    }

    #[test]
    fn std_terminal_refuses_a_redirected_stdout_or_stdin() {
        let (socket, _keep) = socket_file();
        let redirected_out = StdTerminal {
            input: Some(tty_file()),
            output: Some(socket),
        };
        assert!(!redirected_out.is_interactive_terminal());
        let (socket, _keep) = socket_file();
        let redirected_in = StdTerminal {
            input: Some(socket),
            output: Some(tty_file()),
        };
        assert!(!redirected_in.is_interactive_terminal());
        let both = StdTerminal {
            input: Some(tty_file()),
            output: Some(tty_file()),
        };
        assert!(both.is_interactive_terminal());
        let closed = StdTerminal {
            input: None,
            output: None,
        };
        assert!(!closed.is_interactive_terminal());
    }

    #[test]
    fn prompt_is_written_only_to_the_checked_output_descriptor() {
        let (output, mut peer) = socket_file();
        let mut terminal = StdTerminal {
            input: None,
            output: Some(output),
        };
        terminal.show_prompt("hello CODE").expect("write");
        drop(terminal);
        let mut shown = String::new();
        peer.read_to_string(&mut shown).expect("read");
        assert_eq!(shown, "hello CODE\n");
        let mut no_output = StdTerminal {
            input: None,
            output: None,
        };
        assert!(no_output.show_prompt("x").is_err());
    }
}
