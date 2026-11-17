// SPDX-License-Identifier: MIT
//! A tiny pinentry front-end for rbw's agent.
//!
//! rbw asks an external pinentry program for the master password. q-bw puts a
//! symlink to itself named `pinentry` first on the agent's `PATH`, so this mode
//! answers the Assuan exchange with q-bw's own r-modals password dialog. The
//! password goes straight back over rbw's pipe: it is never an argument, an
//! environment variable or a file.

use std::io::{BufRead, Write};

use crate::paths::Paths;
use crate::ui;

/// Pinentry error code for "cancelled" (`GPG_ERR_CANCELED`).
const CANCELLED: &str = "ERR 83886179 cancelled";

/// `--pinentry` / argv[0] == `pinentry`.
pub fn run(paths: &Paths) -> ! {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve(
        stdin.lock(),
        stdout.lock(),
        || ui::ask_password(paths, "Master password"),
        |event| paths.set_pinentry_result(event),
    );
    std::process::exit(0)
}

/// Serve the pinentry exchange.
///
/// `ask` returns `Some(password)` when the user submitted one and `None` when
/// cancelling. `record` receives `submitted`, `cancelled` or `retry` so the
/// caller can tell a cancellation from rbw's automatic retry after a wrong
/// password (both surface to rbw as the same pinentry error).
fn serve<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    mut ask: impl FnMut() -> Option<String>,
    mut record: impl FnMut(&str),
) {
    // Real pinentry starts with a greeting. rbw's reader counts it as one of
    // the expected `OK` lines, so it must be sent.
    let _ = writeln!(output, "OK Pleased to meet you");
    let _ = output.flush();

    let mut retrying = false;
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let command = line.split_whitespace().next().unwrap_or("");
        match command {
            // rbw sets this before re-prompting after an incorrect password.
            // Stop its retry loop so the user gets one prompt and one error,
            // which is what q-bw's old `bw unlock` flow did.
            "SETERROR" => {
                retrying = true;
                let _ = writeln!(output, "OK");
                let _ = output.flush();
            }
            "GETPIN" => {
                if retrying {
                    record("retry");
                    let _ = writeln!(output, "{CANCELLED}");
                } else {
                    match ask().filter(|password| !password.is_empty()) {
                        Some(password) => {
                            record("submitted");
                            let _ = writeln!(
                                output,
                                "D {}",
                                String::from_utf8_lossy(&encode(password.as_bytes()))
                            );
                            let _ = writeln!(output, "OK");
                        }
                        None => {
                            record("cancelled");
                            let _ = writeln!(output, "{CANCELLED}");
                        }
                    }
                }
                let _ = output.flush();
                // rbw closes stdin after GETPIN; leaving now lets its
                // `child.wait()` return without waiting for another read.
                break;
            }
            "BYE" => {
                let _ = writeln!(output, "OK");
                let _ = output.flush();
                break;
            }
            _ => {
                let _ = writeln!(output, "OK");
                let _ = output.flush();
            }
        }
    }
}

/// Percent-encode the password for the pinentry `D` line.
///
/// `%`, newlines and other control bytes must be escaped; ordinary UTF-8 bytes
/// pass through unchanged. rbw percent-decodes the value back to the exact
/// bytes the user typed.
fn encode(password: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(password.len());
    for &byte in password {
        if byte == b'%' || byte < 0x20 || byte == 0x7f {
            out.push(b'%');
            out.push(HEX[(byte >> 4) as usize]);
            out.push(HEX[(byte & 0x0f) as usize]);
        } else {
            out.push(byte);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{encode, serve};

    fn exchange(input: &str, password: Option<&str>) -> (String, Vec<String>) {
        let mut out = Vec::new();
        let mut events = Vec::new();
        let password = password.map(str::to_string);
        serve(
            Cursor::new(input.as_bytes().to_vec()),
            &mut out,
            || password.clone(),
            |event| events.push(event.to_string()),
        );
        (String::from_utf8(out).unwrap(), events)
    }

    #[test]
    fn password_is_percent_encoded() {
        assert_eq!(encode(b"s3cret"), b"s3cret");
        assert_eq!(encode(b"100%secret"), b"100%25secret");
        assert_eq!(encode(b"line1\nline2\r"), b"line1%0Aline2%0D");
        // UTF-8 bytes are passed through unchanged.
        assert_eq!(encode("pässwörd".as_bytes()), "pässwörd".as_bytes());
    }

    #[test]
    fn submitted_password_answers_the_assuan_exchange() {
        let input = "SETTITLE rbw\nSETPROMPT Master Password\nSETDESC Unlock\nGETPIN\n";
        let (out, events) = exchange(input, Some("p a 100%"));

        // The greeting, one OK per SET command, and the final OK.
        assert_eq!(out.matches("OK").count(), 5);
        assert!(out.contains("D p a 100%25\n"));
        assert_eq!(events, ["submitted"]);
    }

    #[test]
    fn cancellation_is_reported_with_the_pinentry_cancel_code() {
        let input = "SETTITLE rbw\nGETPIN\n";
        let (out, events) = exchange(input, None);

        assert!(out.contains("ERR 83886179 cancelled\n"));
        assert_eq!(events, ["cancelled"]);
    }

    #[test]
    fn a_retry_stops_rbw_from_asking_a_second_time() {
        let input = "SETTITLE rbw\nSETERROR Incorrect password\nGETPIN\n";
        let (out, events) = exchange(input, Some("ignored"));

        assert!(out.contains("ERR 83886179 cancelled\n"));
        assert!(!out.contains("D ignored"), "no second password prompt");
        assert_eq!(events, ["retry"]);
    }
}
