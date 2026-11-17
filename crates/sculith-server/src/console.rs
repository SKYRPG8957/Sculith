//! Local operator input, independent of in-game commands and plugin permissions.
use std::io::BufRead;
use tokio::sync::mpsc;

const MAX_LINE: usize = 256;
const QUEUE: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    List,
    Save,
    Stop,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    Command(Command),
    Plugin(String),
    Invalid,
    TooLong,
    ReadError,
}

pub fn start() -> std::io::Result<mpsc::Receiver<Input>> {
    let (send, receive) = mpsc::channel(QUEUE);
    // Tokio stdin uses an uncancellable blocking-pool read, which can prevent
    // runtime shutdown. A dedicated native thread instead ends with the process;
    // stop/EOF/channel closure finish it normally. EOF does not stop the server.
    std::thread::Builder::new()
        .name("console".into())
        .spawn(move || {
            read(std::io::stdin().lock(), send);
        })?;
    Ok(receive)
}

fn read(mut reader: impl BufRead, send: mpsc::Sender<Input>) {
    let mut line = Vec::with_capacity(MAX_LINE);
    let mut oversized = false;
    loop {
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(_) => {
                let _ = send.blocking_send(Input::ReadError);
                return;
            }
        };
        let eof = buffer.is_empty();
        let newline = buffer.iter().position(|&b| b == b'\n');
        let count = newline.map_or(buffer.len(), |offset| offset + 1);
        if !oversized {
            if line.len() + count > MAX_LINE {
                oversized = true;
                line.clear();
            } else {
                line.extend_from_slice(&buffer[..count]);
            }
        }
        reader.consume(count);
        if newline.is_none() && !eof {
            continue;
        }
        let input = if oversized {
            Some(Input::TooLong)
        } else {
            match std::str::from_utf8(&line).map(str::trim) {
                Ok("") => None,
                Ok(text) if sculith_core::validate_player_text(text).is_ok() => {
                    let text = text.strip_prefix('/').unwrap_or(text);
                    let name = text.split_whitespace().next().unwrap_or("");
                    Some(match text {
                        "help" | "?" => Input::Command(Command::Help),
                        "list" => Input::Command(Command::List),
                        "save" => Input::Command(Command::Save),
                        "stop" => Input::Command(Command::Stop),
                        _ if matches!(name, "list" | "save" | "stop" | "?") || name.is_empty() => {
                            Input::Invalid
                        }
                        _ => Input::Plugin(text.into()),
                    })
                }
                _ => Some(Input::Invalid),
            }
        };
        if let Some(input) = input {
            let stop = input == Input::Command(Command::Stop);
            if send.blocking_send(input).is_err() || stop {
                return;
            }
        }
        line.clear();
        oversized = false;
        if eof {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_framing_resumes_after_oversize_and_invalid_utf8_and_accepts_eof_line() {
        let (send, mut receive) = mpsc::channel(8);
        let mut input = vec![b'x'; 100_000];
        input.extend_from_slice(b"\n\xff\n\r\nlist\r\nsave extra\nsave");
        read(
            std::io::BufReader::with_capacity(17, input.as_slice()),
            send,
        );
        let mut events = Vec::new();
        while let Ok(event) = receive.try_recv() {
            events.push(event);
        }
        assert_eq!(
            events,
            vec![
                Input::TooLong,
                Input::Invalid,
                Input::Command(Command::List),
                Input::Invalid,
                Input::Command(Command::Save)
            ]
        );
    }
    #[test]
    fn stop_ends_reader_without_another_stdin_read_and_empty_eof_is_quiet() {
        let (send, mut receive) = mpsc::channel(8);
        read(&b"help\nstop\nsave\n"[..], send);
        assert_eq!(receive.try_recv().unwrap(), Input::Command(Command::Help));
        assert_eq!(receive.try_recv().unwrap(), Input::Command(Command::Stop));
        assert!(receive.try_recv().is_err());
        let (send, mut receive) = mpsc::channel(8);
        read(&b""[..], send);
        assert!(receive.try_recv().is_err());
    }
}
