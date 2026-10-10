//! One bounded terminal-color query. Raw bytes are read before Crossterm's
//! event reader starts so OSC replies are not discarded by its parser.

use std::io;

use crossterm::{
    event::{Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::Print,
};

pub(crate) struct ProbeResult {
    pub palette: Option<(String, String)>,
    pub events: Vec<Event>,
    pub keyboard_supported: bool,
}

pub(crate) async fn query() -> io::Result<ProbeResult> {
    execute!(
        io::stdout(),
        Print("\x1b[?u\x1b[c\x1b]10;?\x1b\\\x1b]11;?\x1b\\")
    )?;
    #[cfg(unix)]
    let bytes = tokio::task::spawn_blocking(read_bounded).await??;
    #[cfg(not(unix))]
    let bytes = Vec::new();
    let (palette, events) = decode_terminal_probe_bytes(&bytes);
    Ok(ProbeResult {
        palette,
        events,
        keyboard_supported: keyboard_protocol_response(&bytes) == Some(true),
    })
}

/// Decode a captured terminal reply buffer and preserve unrelated key input.
/// This is public only so integration tests can exercise the startup boundary.
pub fn decode_terminal_probe_bytes(input: &[u8]) -> (Option<(String, String)>, Vec<Event>) {
    let (foreground, background, remaining) = extract_palette(input);
    (foreground.zip(background), parse_events(&remaining))
}

/// Decode fragmented terminal input as it arrives, preserving unrelated keys.
#[doc(hidden)]
pub fn decode_terminal_probe_chunks<'a>(
    chunks: impl IntoIterator<Item = &'a [u8]>,
) -> (Option<(String, String)>, Vec<Event>) {
    let mut buffer = Vec::new();
    let mut foreground = None;
    let mut background = None;
    for chunk in chunks {
        buffer.extend_from_slice(chunk);
        let (fg, bg, remaining) = extract_palette(&buffer);
        foreground = fg.or(foreground);
        background = bg.or(background);
        buffer = remaining;
    }
    (foreground.zip(background), parse_events(&buffer))
}

#[cfg(unix)]
fn read_bounded() -> io::Result<Vec<u8>> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_millis(180);
    let mut bytes = Vec::new();
    let mut foreground = false;
    let mut background = false;
    while Instant::now() < deadline
        && (!(foreground && background) || keyboard_protocol_response(&bytes).is_none())
        && bytes.len() < 64 * 1024
    {
        let mut descriptor = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().min(40) as i32;
        // SAFETY: `descriptor` points to one initialized pollfd for the stdin
        // descriptor, and the call does not retain its pointer.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 || descriptor.revents & libc::POLLIN == 0 {
            continue;
        }
        let mut chunk = [0u8; 4096];
        // SAFETY: `chunk` is writable for its full length and stdin is a valid
        // file descriptor. The returned length is checked before extending.
        let count = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count as usize]);
        let (fg, bg, _) = extract_palette(&bytes);
        foreground |= fg.is_some();
        background |= bg.is_some();
    }
    Ok(bytes)
}

fn extract_palette(input: &[u8]) -> (Option<String>, Option<String>, Vec<u8>) {
    let mut foreground = None;
    let mut background = None;
    let mut remaining = Vec::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        if input[cursor..].starts_with(b"\x1b]")
            && let Some(end) = osc_end(input, cursor + 2)
        {
            let payload = &input[cursor + 2..end.payload_end];
            let color = std::str::from_utf8(payload).ok().and_then(|payload| {
                let (kind, color) = payload.split_once(';')?;
                match kind {
                    "10" => Some((true, color.to_owned())),
                    "11" => Some((false, color.to_owned())),
                    _ => None,
                }
            });
            if let Some((is_foreground, color)) = color {
                if is_foreground {
                    foreground = Some(color);
                } else {
                    background = Some(color);
                }
            }
            cursor = end.sequence_end;
            continue;
        }
        remaining.push(input[cursor]);
        cursor += 1;
    }
    (foreground, background, remaining)
}

struct OscEnd {
    payload_end: usize,
    sequence_end: usize,
}

fn osc_end(bytes: &[u8], start: usize) -> Option<OscEnd> {
    let mut cursor = start;
    while cursor < bytes.len() {
        match bytes[cursor] {
            0x07 => {
                return Some(OscEnd {
                    payload_end: cursor,
                    sequence_end: cursor + 1,
                });
            }
            0x1b if bytes.get(cursor + 1) == Some(&b'\\') => {
                return Some(OscEnd {
                    payload_end: cursor,
                    sequence_end: cursor + 2,
                });
            }
            _ => cursor += 1,
        }
    }
    None
}

fn parse_events(bytes: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == 0x1b {
            if let Some(next) = bytes.get(cursor + 1).copied() {
                if next == b'[' {
                    if bytes[cursor..].starts_with(b"\x1b[200~") {
                        let body_start = cursor + 6;
                        if let Some(relative_end) = bytes[body_start..]
                            .windows(6)
                            .position(|window| window == b"\x1b[201~")
                        {
                            let body_end = body_start + relative_end;
                            events.push(Event::Paste(
                                String::from_utf8_lossy(&bytes[body_start..body_end]).into_owned(),
                            ));
                            cursor = body_end + 6;
                            continue;
                        }
                        break;
                    }
                    if let Some(relative_end) = bytes[cursor + 2..]
                        .iter()
                        .position(|byte| (0x40..=0x7e).contains(byte))
                    {
                        let end = cursor + 2 + relative_end;
                        let sequence = &bytes[cursor + 2..end];
                        let final_byte = bytes[end];
                        if let Some(event) = csi_key(sequence, final_byte) {
                            events.push(event);
                        }
                        cursor = end + 1;
                        continue;
                    }
                    break;
                } else if next == b'O' {
                    if let Some(final_byte) = bytes.get(cursor + 2).copied() {
                        let code = match final_byte {
                            b'P' => Some(KeyCode::F(1)),
                            b'Q' => Some(KeyCode::F(2)),
                            b'R' => Some(KeyCode::F(3)),
                            b'S' => Some(KeyCode::F(4)),
                            _ => None,
                        };
                        if let Some(code) = code {
                            events.push(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
                            cursor += 3;
                            continue;
                        }
                    }
                } else if next == b']' {
                    if let Some(end) = osc_end(bytes, cursor + 2) {
                        cursor = end.sequence_end;
                        continue;
                    }
                    break;
                } else if let Some(character) = char_at(bytes, cursor + 1) {
                    events.push(Event::Key(KeyEvent::new(
                        KeyCode::Char(character),
                        KeyModifiers::ALT,
                    )));
                    cursor += 1 + character.len_utf8();
                    continue;
                }
            }
            events.push(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            cursor += 1;
            continue;
        }
        let (code, modifiers, length) = match byte {
            b'\r' | b'\n' => (KeyCode::Enter, KeyModifiers::NONE, 1),
            b'\t' => (KeyCode::Tab, KeyModifiers::NONE, 1),
            0x7f | 0x08 => (KeyCode::Backspace, KeyModifiers::NONE, 1),
            0x01..=0x1a => (
                KeyCode::Char(char::from(byte + b'a' - 1)),
                KeyModifiers::CONTROL,
                1,
            ),
            _ => {
                if let Some(character) = char_at(bytes, cursor) {
                    (
                        KeyCode::Char(character),
                        KeyModifiers::NONE,
                        character.len_utf8(),
                    )
                } else {
                    cursor += 1;
                    continue;
                }
            }
        };
        events.push(Event::Key(KeyEvent::new(code, modifiers)));
        cursor += length;
    }
    events
}

fn char_at(bytes: &[u8], cursor: usize) -> Option<char> {
    let first = *bytes.get(cursor)?;
    let width = match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    std::str::from_utf8(bytes.get(cursor..cursor.checked_add(width)?)?)
        .ok()?
        .chars()
        .next()
}

fn csi_key(sequence: &[u8], final_byte: u8) -> Option<Event> {
    let text = std::str::from_utf8(sequence).ok()?;
    let (code, modifiers) = match final_byte {
        b'A' => (KeyCode::Up, csi_modifiers(text)),
        b'B' => (KeyCode::Down, csi_modifiers(text)),
        b'C' => (KeyCode::Right, csi_modifiers(text)),
        b'D' => (KeyCode::Left, csi_modifiers(text)),
        b'H' => (KeyCode::Home, csi_modifiers(text)),
        b'F' => (KeyCode::End, csi_modifiers(text)),
        b'P' => (KeyCode::F(1), csi_modifiers(text)),
        b'Q' => (KeyCode::F(2), csi_modifiers(text)),
        b'R' => (KeyCode::F(3), csi_modifiers(text)),
        b'S' => (KeyCode::F(4), csi_modifiers(text)),
        b'~' => parse_tilde_key(text)?,
        b'u' => parse_kitty_key(text)?,
        _ => return None,
    };
    Some(Event::Key(KeyEvent::new(code, modifiers)))
}

fn parse_tilde_key(sequence: &str) -> Option<(KeyCode, KeyModifiers)> {
    let mut params = sequence.split(';');
    let number = params.next()?.parse::<u16>().ok()?;
    let modifiers = params
        .next()
        .and_then(|part| part.parse::<u8>().ok())
        .map_or(KeyModifiers::NONE, csi_modifier_number);
    let code = match number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11 => KeyCode::F(1),
        12 => KeyCode::F(2),
        13 => KeyCode::F(3),
        14 => KeyCode::F(4),
        15 => KeyCode::F(5),
        17 => KeyCode::F(6),
        18 => KeyCode::F(7),
        19 => KeyCode::F(8),
        20 => KeyCode::F(9),
        21 => KeyCode::F(10),
        23 => KeyCode::F(11),
        24 => KeyCode::F(12),
        _ => return None,
    };
    Some((code, modifiers))
}

fn parse_kitty_key(sequence: &str) -> Option<(KeyCode, KeyModifiers)> {
    let mut params = sequence.split(';');
    let key = params.next()?.split(':').next()?.parse::<u32>().ok()?;
    let modifier_number = params
        .next()
        .and_then(|part| part.split(':').next())
        .and_then(|part| part.parse::<u8>().ok())
        .unwrap_or(1);
    let modifiers = kitty_modifier_number(modifier_number);
    let code = match key {
        9 => KeyCode::Tab,
        13 => KeyCode::Enter,
        27 => KeyCode::Esc,
        127 => KeyCode::Backspace,
        57364..=57387 => KeyCode::F((key - 57363) as u8),
        _ => KeyCode::Char(char::from_u32(key)?),
    };
    Some((code, modifiers))
}

fn csi_modifier_number(number: u8) -> KeyModifiers {
    match number {
        2 => KeyModifiers::SHIFT,
        3 => KeyModifiers::ALT,
        4 => KeyModifiers::ALT | KeyModifiers::SHIFT,
        5 => KeyModifiers::CONTROL,
        6 => KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        7 => KeyModifiers::ALT | KeyModifiers::CONTROL,
        8 => KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    }
}

fn kitty_modifier_number(number: u8) -> KeyModifiers {
    let bits = number.saturating_sub(1);
    let mut modifiers = KeyModifiers::NONE;
    if bits & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    if bits & 8 != 0 {
        modifiers |= KeyModifiers::SUPER;
    }
    modifiers
}

fn csi_modifiers(sequence: &str) -> KeyModifiers {
    match sequence
        .rsplit(';')
        .next()
        .and_then(|part| part.parse::<u8>().ok())
    {
        Some(number) => csi_modifier_number(number),
        None => KeyModifiers::NONE,
    }
}

fn keyboard_protocol_response(input: &[u8]) -> Option<bool> {
    let mut cursor = 0;
    while cursor + 2 < input.len() {
        if input[cursor..].starts_with(b"\x1b[")
            && let Some(relative_end) = input[cursor + 2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
        {
            let end = cursor + 2 + relative_end;
            let sequence = &input[cursor + 2..end];
            match (sequence.first(), input[end]) {
                (Some(b'?'), b'u') if sequence[1..].iter().all(|byte| byte.is_ascii_digit()) => {
                    return Some(true);
                }
                (Some(b'?'), b'c') => return Some(false),
                _ => {}
            }
            cursor = end + 1;
        } else {
            cursor += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_handles_fragmented_colors_and_replays_interleaved_keys() {
        let chunks: &[&[u8]] = &[
            b"k\x1b]10;rgb:ffff/aaaa/0000\x1b",
            b"\\\x1b[A\x1b]11;rgb:0000/1111/2222",
            b"\x07x",
        ];
        let (palette, events) = decode_terminal_probe_chunks(chunks.iter().copied());
        assert_eq!(
            palette.as_ref().map(|colors| colors.0.as_str()),
            Some("rgb:ffff/aaaa/0000")
        );
        assert_eq!(
            palette.as_ref().map(|colors| colors.1.as_str()),
            Some("rgb:0000/1111/2222")
        );
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], Event::Key(key) if key.code == KeyCode::Char('k')));
        assert!(matches!(events[1], Event::Key(key) if key.code == KeyCode::Up));
        assert!(matches!(events[2], Event::Key(key) if key.code == KeyCode::Char('x')));
    }

    #[test]
    fn complete_unknown_osc_is_consumed_and_incomplete_sequences_are_preserved() {
        let bytes = b"\x1b]9;notify\x07\x1b]10;rgb:aaaa";
        let (_, _, replay) = extract_palette(bytes);
        assert_eq!(replay, b"\x1b]10;rgb:aaaa");
    }

    #[test]
    fn captured_probe_buffer_retains_colors_and_key_replay() {
        let input = b"k\x1b]10;rgb:ffff/aaaa/0000\x1b\\\x1b]11;rgb:0000/1111/2222\x07x";
        let (palette, events) = decode_terminal_probe_bytes(input);
        assert_eq!(
            palette,
            Some(("rgb:ffff/aaaa/0000".into(), "rgb:0000/1111/2222".into()))
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Event::Key(key) if key.code == KeyCode::Char('k')));
        assert!(matches!(events[1], Event::Key(key) if key.code == KeyCode::Char('x')));
    }

    #[test]
    fn startup_replay_preserves_function_keys_and_kitty_control_shift_chords() {
        let input = b"\x1b[?1u\x1b[21~\x1b[24~\x1b[5~\x1b[106;6u";
        assert_eq!(keyboard_protocol_response(input), Some(true));
        let (_, events) = decode_terminal_probe_bytes(input);
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], Event::Key(key) if key.code == KeyCode::F(10)));
        assert!(matches!(events[1], Event::Key(key) if key.code == KeyCode::F(12)));
        assert!(matches!(events[2], Event::Key(key) if key.code == KeyCode::PageUp));
        assert!(matches!(events[3], Event::Key(key)
            if key.code == KeyCode::Char('j')
                && key.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)));
    }

    #[test]
    fn unsupported_keyboard_protocol_reply_does_not_enable_enhancements() {
        assert_eq!(keyboard_protocol_response(b"\x1b[?62c"), Some(false));
        assert_eq!(keyboard_protocol_response(b"\x1b[?1u"), Some(true));
        assert_eq!(keyboard_protocol_response(b"\x1b[A"), None);
    }

    #[test]
    fn paste_and_unknown_control_packets_never_become_hotkeys() {
        let events = parse_events(b"\x1b[200~q\x1b[A\x1b[201~\x1b[999zok");
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], Event::Paste(text) if text == "q\x1b[A"));
        assert!(matches!(events[1], Event::Key(key) if key.code == KeyCode::Char('o')));
        assert!(matches!(events[2], Event::Key(key) if key.code == KeyCode::Char('k')));
        assert!(matches!(parse_events(b"\x1b[200~q").as_slice(), []));
    }

    #[test]
    fn valid_unicode_before_incomplete_trailing_scalar_is_preserved() {
        assert_eq!(parse_events(&[b'a', 0xf0, 0x9f]).len(), 1);
        assert!(
            matches!(parse_events(&[b'a', 0xf0, 0x9f]).as_slice(), [Event::Key(key)] if key.code == KeyCode::Char('a'))
        );
    }
}
