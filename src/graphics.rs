//! Kitty graphics frames from graphics previewers, and the command that
//! takes them off the screen.
//!
//! A graphics previewer's stdout is a stream of frames. A frame is optional
//! relative cursor moves (`CSI n A`, `B`, `C` or `D`) and then one image: a
//! kitty graphics transmit-and-display command (`ESC _ G a=T,... ESC \`),
//! chunked with `m=1` on every chunk but the last. vfm puts the cursor on the
//! pane's top-left cell before forwarding a frame, so the moves position the
//! image inside the pane.
//!
//! [`Splitter`] cuts the stream into frames and polices it, since vfm
//! forwards the bytes to the terminal. Only cursor moves and the image's
//! commands get through, so a previewer can't clear, scroll or reconfigure
//! the screen. Every command is made quiet (`q=2`): a terminal reply would
//! reach vfm's input as keystrokes. The image is made not to move the cursor
//! (`C=1`), which could scroll the screen. Text outside escape sequences is
//! kept, up to [`MAX_TEXT`] bytes, as the message of a previewer that shows
//! nothing.

// Rust guideline compliant 2026-10-07

use std::io::Write;

const ESC: u8 = 0x1b;

/// Largest frame kept, in bytes; larger ones are dropped whole.
///
/// A full-screen RGBA frame on a 5K display is about 80 MB as base64, but a
/// preview pane is at most half the screen. The cap bounds memory when a
/// previewer never ends its image.
pub const MAX_FRAME: usize = 64 << 20;

/// Most text kept from outside escape sequences: room for an error line.
pub const MAX_TEXT: usize = 512;

/// Most control-data bytes parsed per command. Real ones are under 100.
const MAX_CONTROL: usize = 256;

/// Longest cursor move kept, `ESC [` excluded; longer CSIs are dropped.
const MAX_CSI: usize = 8;

/// One image, ready for the terminal once the cursor is at the pane origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The image's `i=` id, which deletes it later; `None` if it has none.
    pub id: Option<u32>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    /// After ESC.
    Escape,
    /// Inside a CSI; `csi` holds its bytes after `ESC [`.
    Csi,
    /// After `ESC _`: a `G` makes it a graphics command.
    ApcStart,
    /// Inside a graphics command's control data, before its `;`.
    Control,
    /// Inside a graphics command's base64 payload.
    Payload,
    /// ESC inside a graphics command; `\` ends the command.
    CommandEsc,
    /// Inside a string sequence being dropped (OSC, DCS, other APCs, ...).
    Skip,
    /// ESC inside a dropped string sequence; `\` ends it.
    SkipEsc,
}

/// Cuts a graphics previewer's output into [`Frame`]s, chunk by chunk.
#[derive(Debug, Default)]
pub struct Splitter {
    state: State,
    csi: Vec<u8>,
    /// Kept bytes of the frame being assembled.
    frame: Vec<u8>,
    /// Where the current command starts in `frame`, to drop it if malformed.
    command_start: usize,
    /// Control data of the current command.
    control: Vec<u8>,
    /// A chunked image is underway: commands continue it, not start one.
    in_image: bool,
    id: Option<u32>,
    /// The frame outgrew [`MAX_FRAME`]; it is dropped when its image ends.
    oversized: bool,
    text: Vec<u8>,
}

impl Splitter {
    /// Consumes `data`, appending each frame it completes to `frames`.
    pub fn feed(&mut self, data: &[u8], frames: &mut Vec<Frame>) {
        let mut i = 0;
        while i < data.len() {
            if self.state == State::Payload {
                // Payloads are megabytes of base64: copy runs, not bytes.
                let run = data[i..].iter().position(|&b| b == ESC).unwrap_or(data.len() - i);
                self.keep(&data[i..i + run]);
                i += run;
                if i < data.len() {
                    self.state = State::CommandEsc;
                    i += 1;
                }
            } else if self.step(data[i], frames) {
                i += 1;
            }
        }
    }

    /// Advances the state machine by `byte`, outside payloads. Returns false
    /// when `byte` ended a sequence without belonging to it and must be read
    /// again as the start of the next one.
    fn step(&mut self, byte: u8, frames: &mut Vec<Frame>) -> bool {
        match self.state {
            State::Payload => unreachable!("feed copies payloads in runs"),
            State::Ground => {
                if byte == ESC {
                    self.state = State::Escape;
                } else if self.text.len() < MAX_TEXT && (byte >= 0x20 || byte == b'\n') {
                    self.text.push(byte);
                }
            }
            State::Escape => {
                self.state = match byte {
                    b'[' => {
                        self.csi.clear();
                        State::Csi
                    }
                    b'_' => State::ApcStart,
                    b']' | b'P' | b'^' | b'X' => State::Skip,
                    ESC => State::Escape,
                    _ => State::Ground, // a two-byte sequence: dropped
                };
            }
            State::Csi => self.csi_byte(byte),
            State::ApcStart => {
                self.state = match byte {
                    b'G' => {
                        self.command_start = self.frame.len();
                        self.control.clear();
                        self.keep(b"\x1b_G");
                        State::Control
                    }
                    ESC => State::SkipEsc,
                    _ => State::Skip,
                };
            }
            State::Control => self.control_byte(byte),
            State::CommandEsc => {
                if byte != b'\\' {
                    // The command was cut short, so its image is broken.
                    self.drop_image();
                    self.state = State::Escape;
                    return false;
                }
                self.keep(b"\x1b\\");
                self.finish_command(frames);
                self.state = State::Ground;
            }
            State::Skip => match byte {
                ESC => self.state = State::SkipEsc,
                0x07 => self.state = State::Ground,
                _ => {}
            },
            State::SkipEsc => {
                if byte != b'\\' {
                    // The string ended; this byte starts a new sequence.
                    self.state = State::Escape;
                    return false;
                }
                self.state = State::Ground;
            }
        }
        true
    }

    /// A byte inside a CSI: only digits and a cursor move are kept.
    fn csi_byte(&mut self, byte: u8) {
        match byte {
            0x20..=0x3f if self.csi.len() < MAX_CSI => self.csi.push(byte),
            0x40..=0x7e => {
                let moves = matches!(byte, b'A' | b'B' | b'C' | b'D');
                if moves && self.csi.iter().all(u8::is_ascii_digit) {
                    let csi = std::mem::take(&mut self.csi);
                    self.keep(b"\x1b[");
                    self.keep(&csi);
                    self.keep(&[byte]);
                    self.csi = csi;
                }
                self.state = State::Ground;
            }
            ESC => self.state = State::Escape,
            _ => self.state = State::Ground, // malformed or too long: dropped
        }
    }

    /// A byte of a graphics command's control data, or the `;`/ESC ending it.
    fn control_byte(&mut self, byte: u8) {
        match byte {
            b';' | ESC => {
                self.state = match (self.close_control(), byte) {
                    (true, b';') => {
                        self.keep(b";");
                        State::Payload
                    }
                    (true, _) => State::CommandEsc,
                    (false, ESC) => State::SkipEsc,
                    (false, _) => State::Skip,
                };
            }
            _ => {
                if self.control.len() < MAX_CONTROL {
                    self.control.push(byte);
                }
                self.keep(&[byte]);
            }
        }
    }

    /// The first line of text the previewer printed, if any.
    pub fn message(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.text);
        text.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from)
    }

    fn keep(&mut self, bytes: &[u8]) {
        if self.frame.len() + bytes.len() > MAX_FRAME {
            self.oversized = true;
        }
        if !self.oversized {
            self.frame.extend_from_slice(bytes);
        }
    }

    /// Ends the current command's control data, forcing its quiet keys.
    ///
    /// Returns false, having dropped the command, unless it starts an image
    /// (`a=T`) or continues the one underway. A continuation may only carry
    /// chunking keys, so one with an action breaks the image underway.
    fn close_control(&mut self) -> bool {
        if self.in_image {
            if key(&self.control, b'a').is_some() {
                self.drop_image();
                return false;
            }
        } else {
            if key(&self.control, b'a') != Some(b"T") {
                self.frame.truncate(self.command_start);
                return false;
            }
            self.id =
                key(&self.control, b'i').and_then(|v| std::str::from_utf8(v).ok()?.parse().ok()).filter(|&id| id != 0);
        }
        // Later keys win, so appending overrides whatever the previewer set.
        let sep = if self.control.is_empty() { "" } else { "," };
        let forced = if self.in_image { "q=2" } else { "q=2,C=1" };
        self.keep(format!("{sep}{forced}").as_bytes());
        true
    }

    fn finish_command(&mut self, frames: &mut Vec<Frame>) {
        self.in_image = key(&self.control, b'm') == Some(b"1");
        if self.in_image {
            return;
        }
        let (id, bytes) = (self.id.take(), std::mem::take(&mut self.frame));
        if !std::mem::take(&mut self.oversized) {
            frames.push(Frame { id, bytes });
        }
    }

    fn drop_image(&mut self) {
        self.frame.clear();
        self.in_image = false;
        self.oversized = false;
        self.id = None;
    }
}

/// The value of single-letter key `name` in graphics control data.
///
/// The last occurrence wins, as in terminals.
fn key(control: &[u8], name: u8) -> Option<&[u8]> {
    control
        .split(|&b| b == b',')
        .filter_map(|kv| match kv {
            [k, b'=', value @ ..] if *k == name => Some(value),
            _ => None,
        })
        .next_back()
}

/// Appends the command deleting image `id` from the screen and from memory,
/// or every image on screen if `id` is `None`.
pub fn delete(buf: &mut Vec<u8>, id: Option<u32>) {
    let _ = match id {
        Some(id) => write!(buf, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\"),
        None => write!(buf, "\x1b_Ga=d,d=A,q=2\x1b\\"),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_bytewise(data: &[u8]) -> (Vec<Frame>, Splitter) {
        let mut s = Splitter::default();
        let mut frames = Vec::new();
        for b in data {
            s.feed(std::slice::from_ref(b), &mut frames);
        }
        (frames, s)
    }

    fn split(data: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        Splitter::default().feed(data, &mut frames);
        // Byte by byte must give the same result as one chunk.
        assert_eq!(split_bytewise(data).0, frames);
        frames
    }

    /// kitim's output for one 2-chunk frame, centered 3 cells right.
    const KITIM: &[u8] = b"\x1b[3C\x1b_Ga=T,i=77,f=24,s=2,v=1,c=1,r=1,C=1,q=2,m=1;AAAA\x1b\\\x1b_Gq=2,m=0;AAAA\x1b\\";

    #[test]
    fn chunked_image_is_one_frame_with_its_id_and_forced_keys() {
        let frames = split(KITIM);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, Some(77));
        let want =
            b"\x1b[3C\x1b_Ga=T,i=77,f=24,s=2,v=1,c=1,r=1,C=1,q=2,m=1,q=2,C=1;AAAA\x1b\\\x1b_Gq=2,m=0,q=2;AAAA\x1b\\";
        assert_eq!(String::from_utf8_lossy(&frames[0].bytes), String::from_utf8_lossy(want));
    }

    #[test]
    fn frames_come_one_per_image_and_unquiet_commands_are_silenced() {
        let mut data = KITIM.to_vec();
        data.extend_from_slice(b"\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\");
        let frames = split(&data);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].id, None);
        assert_eq!(frames[1].bytes, b"\x1b_Ga=T,f=24,s=1,v=1,q=2,C=1;AAAA\x1b\\");
    }

    #[test]
    fn screen_changing_sequences_and_other_commands_are_dropped() {
        let mut data = b"\x1b[2J\x1b[1;1H\x1b[?1049l\x1b]0;title\x07\x1bPdcs\x1b\\\x1b_Xother\x1b\\".to_vec();
        data.extend_from_slice(b"\x1b_Ga=d,d=A\x1b\\\x1b_Ga=q,i=5;AAAA\x1b\\");
        data.extend_from_slice(b"\x1b[12B\x1b_Ga=T,i=9;AAAA\x1b\\");
        let frames = split(&data);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].bytes, b"\x1b[12B\x1b_Ga=T,i=9,q=2,C=1;AAAA\x1b\\");
    }

    #[test]
    fn a_cut_short_command_drops_its_image_but_not_the_next() {
        let mut data = b"\x1b_Ga=T,i=1,m=1;AA\x1b[A".to_vec();
        data.extend_from_slice(b"\x1b_Ga=T,i=2;AAAA\x1b\\");
        let frames = split(&data);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, Some(2));
        assert!(frames[0].bytes.starts_with(b"\x1b[A\x1b_G"), "{:?}", frames[0].bytes);
    }

    #[test]
    fn an_action_inside_a_chunked_image_breaks_it() {
        let mut data = b"\x1b_Ga=T,i=1,m=1;AAAA\x1b\\\x1b_Ga=d,d=A\x1b\\".to_vec();
        data.extend_from_slice(b"\x1b_Gm=0;AAAA\x1b\\\x1b_Ga=T,i=2;AAAA\x1b\\");
        let frames = split(&data);
        assert_eq!(frames.iter().map(|f| f.id).collect::<Vec<_>>(), [Some(2)]);
    }

    #[test]
    fn oversized_images_are_dropped() {
        let mut s = Splitter::default();
        let mut frames = Vec::new();
        s.feed(b"\x1b_Ga=T,i=3;", &mut frames);
        s.feed(&vec![b'A'; MAX_FRAME], &mut frames);
        s.feed(b"\x1b\\\x1b_Ga=T,i=4;AAAA\x1b\\", &mut frames);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, Some(4));
    }

    #[test]
    fn plain_text_is_the_message() {
        let (frames, s) = split_bytewise(b"\n  no video stream \nmore\n");
        assert!(frames.is_empty());
        assert_eq!(s.message().as_deref(), Some("no video stream"));
        assert_eq!(Splitter::default().message(), None);
    }

    #[test]
    fn delete_commands_are_quiet() {
        let mut buf = Vec::new();
        delete(&mut buf, Some(42));
        delete(&mut buf, None);
        assert_eq!(buf, b"\x1b_Ga=d,d=I,i=42,q=2\x1b\\\x1b_Ga=d,d=A,q=2\x1b\\");
    }
}
