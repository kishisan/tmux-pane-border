use std::fmt::Write;

/// Filters VT sequences from child output, applying coordinate offsets.
///
/// This module parses output byte-by-byte using a simple state machine to identify
/// CSI sequences that contain absolute coordinates, and offsets them by +1 to account
/// for the border frame.
///
/// We use a hand-rolled parser instead of the `vte` crate for the output filter because
/// we need to transform sequences in-place while forwarding all other bytes verbatim.

/// State machine for parsing VT/CSI sequences in the output stream.
///
/// This must be persisted across calls to `filter_child_output` so that
/// sequences split across read boundaries are handled correctly.
pub enum FilterState {
    /// Normal text passthrough
    Ground,
    /// Just saw ESC (0x1B)
    Escape,
    /// Inside a CSI sequence (ESC [ or 0x9B), collecting parameters
    Csi {
        params: Vec<u8>,
    },
    /// Inside an OSC sequence (ESC ]) - pass through until ST
    Osc,
    /// Inside an OSC sequence and just saw ESC - waiting for '\' to complete ST
    OscEscape,
}

impl FilterState {
    pub fn new() -> Self {
        FilterState::Ground
    }
}

/// Border rendering info needed to redraw individual border characters
/// after erase operations that damage the border.
pub struct BorderInfo<'a> {
    /// The vertical border character (e.g., '│')
    pub vertical_char: char,
    /// The ANSI color sequence for the border (e.g., "\x1b[38;2;r;g;bm")
    pub color_seq: &'a str,
}

/// Process child PTY output, offsetting absolute coordinates for the border.
/// Returns the transformed bytes to write to the outer terminal.
///
/// `state` must be persisted across calls so that sequences split across
/// read boundaries are parsed correctly.
pub fn filter_child_output(input: &[u8], outer_width: u16, outer_height: u16, border_info: &BorderInfo, state: &mut FilterState) -> Vec<u8> {
    let inner_height = outer_height.saturating_sub(2);
    let inner_width = outer_width.saturating_sub(2);
    let mut output = Vec::with_capacity(input.len() + input.len() / 4);

    for &byte in input {
        match state {
            FilterState::Ground => {
                if byte == 0x1B {
                    *state = FilterState::Escape;
                } else if byte == 0x9B {
                    // 8-bit CSI
                    *state = FilterState::Csi { params: Vec::new() };
                } else if byte == 0x0D {
                    // BUG 1 fix: \r would move cursor to column 1 (left border).
                    // Convert to CHA column 2 (inner left edge) instead.
                    output.extend_from_slice(b"\x1b[2G");
                } else if byte == 0x0A {
                    // LF: pass through, then repair bottom row side borders.
                    // When cursor is on the scroll region bottom row, LF triggers
                    // a scroll that shifts side border characters up, leaving the
                    // new bottom row without borders.
                    output.push(byte);
                    let v = border_info.vertical_char;
                    let color = border_info.color_seq;
                    let reset = "\x1b[0m";
                    let bottom_row = outer_height - 1;
                    let mut repair = String::new();
                    let _ = write!(
                        repair,
                        "\x1b7\x1b[{bottom_row};1H{color}{v}{reset}\x1b[{bottom_row};{outer_width}H{color}{v}{reset}\x1b8",
                    );
                    output.extend_from_slice(repair.as_bytes());
                } else {
                    output.push(byte);
                }
            }
            FilterState::Escape => {
                match byte {
                    b'[' => {
                        *state = FilterState::Csi { params: Vec::new() };
                    }
                    b']' => {
                        // OSC sequence - pass through
                        output.extend_from_slice(b"\x1b]");
                        *state = FilterState::Osc;
                    }
                    _ => {
                        // Other escape sequence (e.g., ESC M for reverse index)
                        output.push(0x1B);
                        output.push(byte);
                        *state = FilterState::Ground;
                    }
                }
            }
            FilterState::Csi { ref mut params } => {
                if byte >= 0x20 && byte <= 0x3F {
                    // Parameter byte or intermediate byte
                    params.push(byte);
                } else if byte >= 0x40 && byte <= 0x7E {
                    // Final byte - process the CSI sequence
                    let transformed = transform_csi(params, byte, inner_width, inner_height, outer_width, border_info);
                    output.extend_from_slice(transformed.as_bytes());
                    *state = FilterState::Ground;
                } else {
                    // Unexpected byte - dump what we have
                    output.extend_from_slice(b"\x1b[");
                    output.extend_from_slice(params);
                    output.push(byte);
                    *state = FilterState::Ground;
                }
            }
            FilterState::Osc => {
                if byte == 0x07 {
                    // BEL terminates OSC
                    output.push(byte);
                    *state = FilterState::Ground;
                } else if byte == 0x1B {
                    // Possible start of ST (ESC \)
                    *state = FilterState::OscEscape;
                } else {
                    output.push(byte);
                }
            }
            FilterState::OscEscape => {
                if byte == b'\\' {
                    // ST (ESC \) terminates OSC
                    output.push(0x1B);
                    output.push(byte);
                    *state = FilterState::Ground;
                } else {
                    // Not ST, emit the ESC and continue in OSC
                    output.push(0x1B);
                    output.push(byte);
                    *state = FilterState::Osc;
                }
            }
        }
    }

    // Incomplete sequences are kept in `state` and will be completed
    // on the next call. No flushing needed here.

    output
}

/// Parse semicolon-separated numeric parameters from CSI param bytes.
fn parse_params(params: &[u8]) -> Vec<u16> {
    let s = std::str::from_utf8(params).unwrap_or("");
    if s.is_empty() {
        return vec![];
    }
    s.split(';')
        .map(|p| p.parse::<u16>().unwrap_or(0))
        .collect()
}

/// Transform a CSI sequence, applying coordinate offsets where needed.
fn transform_csi(params: &[u8], final_byte: u8, inner_width: u16, inner_height: u16, outer_width: u16, border_info: &BorderInfo) -> String {
    let mut result = String::new();

    // Check for private mode prefix (?)
    let (is_private, param_start) = if params.first() == Some(&b'?') {
        (true, 1)
    } else if params.first() == Some(&b'<') {
        // SGR mouse - this is in the input direction, but handle it here for completeness
        // Actually, mouse reports from the child should also be offset
        return transform_sgr_mouse(params, final_byte);
    } else {
        (false, 0)
    };

    let nums = parse_params(&params[param_start..]);

    match final_byte {
        b'H' | b'f' => {
            // CUP / HVP: CSI row ; col H
            let row = nums.first().copied().unwrap_or(1).max(1);
            let col = nums.get(1).copied().unwrap_or(1).max(1);
            // Clamp to inner area, then offset
            let new_row = row.min(inner_height) + 1;
            let new_col = col.min(inner_width) + 1;
            let _ = write!(result, "\x1b[{new_row};{new_col}H");
        }
        b'd' => {
            // VPA: CSI row d
            let row = nums.first().copied().unwrap_or(1).max(1);
            let new_row = row.min(inner_height) + 1;
            let _ = write!(result, "\x1b[{new_row}d");
        }
        b'G' | b'`' => {
            // CHA: CSI col G  /  HPA: CSI col `
            let col = nums.first().copied().unwrap_or(1).max(1);
            let new_col = col.min(inner_width) + 1;
            let _ = write!(result, "\x1b[{new_col}{}", final_byte as char);
        }
        b'r' if !is_private => {
            // DECSTBM: CSI top ; bottom r
            let top = nums.first().copied().unwrap_or(1).max(1);
            let bottom = nums.get(1).copied().unwrap_or(inner_height).min(inner_height);
            let new_top = top + 1;
            let new_bottom = bottom + 1;
            let _ = write!(result, "\x1b[{new_top};{new_bottom}r");
        }
        b'J' => {
            // ED (Erase in Display) - handle carefully to protect borders
            let mode = nums.first().copied().unwrap_or(0);
            match mode {
                0 => {
                    // Erase from cursor to end: cursor position unknown,
                    // so conservatively clear entire inner area.
                    // Save/restore cursor since our row-by-row erase moves it.
                    let _ = write!(result, "\x1b7"); // save cursor
                    for row in 2..=(inner_height + 1) {
                        let _ = write!(result, "\x1b[{row};2H\x1b[{}X", inner_width);
                    }
                    let _ = write!(result, "\x1b8"); // restore cursor
                }
                1 => {
                    // Erase from beginning to cursor: cursor position unknown,
                    // so conservatively clear entire inner area.
                    let _ = write!(result, "\x1b7"); // save cursor
                    for row in 2..=(inner_height + 1) {
                        let _ = write!(result, "\x1b[{row};2H\x1b[{}X", inner_width);
                    }
                    let _ = write!(result, "\x1b8"); // restore cursor
                }
                2 | 3 => {
                    // Erase entire display - we convert to clearing inner area only
                    // by erasing each inner line
                    for row in 2..=(inner_height + 1) {
                        let _ = write!(result, "\x1b[{row};2H\x1b[{}X", inner_width);
                    }
                    // Restore cursor to inner area top-left, matching expected ED 2J behavior
                    let _ = write!(result, "\x1b[2;2H");
                }
                _ => {
                    let _ = write!(result, "\x1b[{mode}J");
                }
            }
        }
        b'K' => {
            // EL (Erase in Line) - pass through but repair damaged borders
            let mode = nums.first().copied().unwrap_or(0);
            let v = border_info.vertical_char;
            let color = border_info.color_seq;
            let reset = "\x1b[0m";
            match mode {
                0 => {
                    // BUG 2 fix: Erase from cursor to end of line.
                    // Pass through CSI K (terminal handles the erase correctly),
                    // then redraw right border character which gets erased.
                    let _ = write!(result, "\x1b[K\x1b7\x1b[{outer_width}G{color}{v}{reset}\x1b8");
                }
                1 => {
                    // BUG 4 fix: Erase from beginning of line to cursor.
                    // CSI 1K erases from column 1, destroying left border.
                    // Pass through then redraw left border character.
                    let _ = write!(result, "\x1b[1K\x1b7\x1b[1G{color}{v}{reset}\x1b8");
                }
                2 => {
                    // Erase entire line: pass through, then redraw both borders.
                    let _ = write!(result, "\x1b[2K\x1b7\x1b[1G{color}{v}{reset}\x1b[{outer_width}G{color}{v}{reset}\x1b8");
                }
                _ => {
                    let param_str = std::str::from_utf8(params).unwrap_or("");
                    let _ = write!(result, "\x1b[{param_str}K");
                }
            }
        }
        b'A' | b'B' | b'C' | b'D' => {
            // Cursor movement (relative) - pass through without offset
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}{}", final_byte as char);
        }
        b'E' => {
            // BUG 3 fix: CNL (Cursor Next Line) moves down then to column 1.
            // Convert to CUD (down) + CHA column 2 to stay inside border.
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}B\x1b[2G");
        }
        b'F' => {
            // BUG 3 fix: CPL (Cursor Previous Line) moves up then to column 1.
            // Convert to CUU (up) + CHA column 2 to stay inside border.
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}A\x1b[2G");
        }
        b'h' | b'l' if is_private => {
            // Private mode set/reset (e.g., ?1049h for alternate screen, ?25h for cursor)
            let param_str = std::str::from_utf8(&params[1..]).unwrap_or("");
            let _ = write!(result, "\x1b[?{param_str}{}", final_byte as char);
            // Note: after alternate screen switch, border will be redrawn by main loop
            // We check for ?1049 and ?47 in the main loop
        }
        _ => {
            // All other CSI sequences - pass through unchanged
            let param_str = std::str::from_utf8(params).unwrap_or("");
            if is_private {
                let _ = write!(result, "\x1b[?{}{}", &param_str[1..], final_byte as char);
            } else {
                let _ = write!(result, "\x1b[{param_str}{}", final_byte as char);
            }
        }
    }

    result
}

/// Transform SGR mouse sequence coordinates.
fn transform_sgr_mouse(params: &[u8], final_byte: u8) -> String {
    // SGR mouse: CSI < btn;col;row M/m
    // These come FROM the child, so we need to ADD offset for outer terminal
    let param_str = std::str::from_utf8(params).unwrap_or("");
    // params starts with '<', skip it
    let nums_str = &param_str[1..];
    let parts: Vec<&str> = nums_str.split(';').collect();

    if parts.len() == 3 {
        let btn = parts[0];
        let col: u16 = parts[1].parse().unwrap_or(1);
        let row: u16 = parts[2].parse().unwrap_or(1);
        // Offset +1 for border
        format!("\x1b[<{btn};{};{}{}",
            col + 1,
            row + 1,
            final_byte as char
        )
    } else {
        // Can't parse, pass through
        format!("\x1b[{param_str}{}", final_byte as char)
    }
}

/// Result of transforming mouse input.
pub enum MouseTransform {
    /// Successfully transformed to inner coordinates.
    Transformed(Vec<u8>),
    /// Click was on the border — should be ignored.
    OnBorder,
    /// Could not parse the sequence — forward original input unchanged.
    ParseError,
}

/// Transform mouse input FROM the outer terminal TO the inner PTY.
/// Removes the border offset from coordinates.
pub fn transform_mouse_input(input: &[u8], outer_width: u16, outer_height: u16) -> MouseTransform {
    // Try to detect SGR mouse format: ESC [ < btn ; col ; row M/m
    let s = match std::str::from_utf8(input) {
        Ok(s) => s,
        Err(_) => return MouseTransform::ParseError,
    };

    if let Some(rest) = s.strip_prefix("\x1b[<") {
        // SGR format
        let end_idx = match rest.find(['M', 'm']) {
            Some(i) => i,
            None => return MouseTransform::ParseError,
        };
        let final_char = rest.as_bytes()[end_idx];
        let params_str = &rest[..end_idx];
        let parts: Vec<&str> = params_str.split(';').collect();

        if parts.len() == 3 {
            let btn = parts[0];
            let col: u16 = match parts[1].parse() {
                Ok(v) => v,
                Err(_) => return MouseTransform::ParseError,
            };
            let row: u16 = match parts[2].parse() {
                Ok(v) => v,
                Err(_) => return MouseTransform::ParseError,
            };

            // Check if click is within inner area
            if col < 2 || col >= outer_width || row < 2 || row >= outer_height {
                return MouseTransform::OnBorder;
            }

            let inner_col = col - 1;
            let inner_row = row - 1;

            return MouseTransform::Transformed(
                format!("\x1b[<{btn};{inner_col};{inner_row}{}", final_char as char).into_bytes(),
            );
        }

        return MouseTransform::ParseError;
    }

    // X10 mouse: ESC [ M Cb Cx Cy (all +32 encoded)
    if input.len() >= 6 && input[0] == 0x1B && input[1] == b'[' && input[2] == b'M' {
        let cb = input[3];
        let cx = input[4];
        let cy = input[5];

        let col = cx.wrapping_sub(32) as u16;
        let row = cy.wrapping_sub(32) as u16;

        if col < 2 || col >= outer_width || row < 2 || row >= outer_height {
            return MouseTransform::OnBorder;
        }

        let inner_col = col - 1 + 32;
        let inner_row = row - 1 + 32;

        // X10 format encodes coordinates in a single byte; values > 255 can't be represented
        if inner_col > 255 || inner_row > 255 {
            return MouseTransform::OnBorder;
        }

        return MouseTransform::Transformed(vec![0x1B, b'[', b'M', cb, inner_col as u8, inner_row as u8]);
    }

    // Not a recognized mouse sequence, forward as-is
    MouseTransform::ParseError
}

/// Check if the output contains an alternate screen switch sequence.
/// Returns true if ?1049h or ?47h is detected (enter alt screen).
pub fn has_alt_screen_enter(data: &[u8]) -> bool {
    // Simple pattern search for common alt screen sequences
    let patterns: &[&[u8]] = &[
        b"\x1b[?1049h",
        b"\x1b[?47h",
    ];
    for pattern in patterns {
        if contains_bytes(data, pattern) {
            return true;
        }
    }
    false
}

/// Check if the output contains an alternate screen exit sequence.
pub fn has_alt_screen_leave(data: &[u8]) -> bool {
    let patterns: &[&[u8]] = &[
        b"\x1b[?1049l",
        b"\x1b[?47l",
    ];
    for pattern in patterns {
        if contains_bytes(data, pattern) {
            return true;
        }
    }
    false
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_border_info() -> BorderInfo<'static> {
        BorderInfo {
            vertical_char: '│',
            color_seq: "\x1b[38;2;97;175;239m",
        }
    }

    fn filter(input: &[u8], outer_width: u16, outer_height: u16, state: &mut FilterState) -> Vec<u8> {
        filter_child_output(input, outer_width, outer_height, &test_border_info(), state)
    }

    #[test]
    fn test_cup_offset() {
        // CSI 1;1 H should become CSI 2;2 H
        let input = b"\x1b[1;1H";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2;2H");
    }

    #[test]
    fn test_cup_default_params() {
        // CSI H (no params) = row 1, col 1
        let input = b"\x1b[H";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2;2H");
    }

    #[test]
    fn test_vpa_offset() {
        let input = b"\x1b[5d";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[6d");
    }

    #[test]
    fn test_cha_offset() {
        let input = b"\x1b[10G";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[11G");
    }

    #[test]
    fn test_decstbm_offset() {
        // CSI 1;22 r → CSI 2;23 r
        let input = b"\x1b[1;22r";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2;23r");
    }

    #[test]
    fn test_relative_movement_passthrough() {
        // Relative cursor movement should pass through unchanged
        let input = b"\x1b[5A";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[5A");
    }

    #[test]
    fn test_plain_text_passthrough() {
        let input = b"Hello, world!";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(&output, input);
    }

    #[test]
    fn test_sgr_passthrough() {
        // Color sequences should pass through
        let input = b"\x1b[31mred\x1b[0m";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[31mred\x1b[0m");
    }

    #[test]
    fn test_mouse_input_transform() {
        // SGR mouse click at col 5, row 3 -> inner col 4, row 2
        let input = b"\x1b[<0;5;3M";
        match transform_mouse_input(input, 80, 24) {
            MouseTransform::Transformed(out) => {
                assert_eq!(std::str::from_utf8(&out).unwrap(), "\x1b[<0;4;2M");
            }
            _ => panic!("expected Transformed"),
        }
    }

    #[test]
    fn test_mouse_input_on_border() {
        // Click at col 1, row 1 (top-left border) should be OnBorder
        let input = b"\x1b[<0;1;1M";
        assert!(matches!(transform_mouse_input(input, 80, 24), MouseTransform::OnBorder));
    }

    #[test]
    fn test_mouse_input_incomplete_sgr() {
        // Incomplete SGR sequence should return ParseError, not drop input
        let input = b"\x1b[<0;5";
        assert!(matches!(transform_mouse_input(input, 80, 24), MouseTransform::ParseError));
    }

    #[test]
    fn test_split_csi_across_calls() {
        // CSI 1;1H split across two reads: first "\x1b[1" then ";1H"
        let bi = test_border_info();
        let mut state = FilterState::new();
        let out1 = filter_child_output(b"\x1b[1", 80, 24, &bi, &mut state);
        let out2 = filter_child_output(b";1H", 80, 24, &bi, &mut state);

        let combined = [out1, out2].concat();
        // Should produce the same result as a single call
        assert_eq!(std::str::from_utf8(&combined).unwrap(), "\x1b[2;2H");
    }

    #[test]
    fn test_split_escape_then_bracket() {
        // ESC at end of first read, '[' at start of second
        let bi = test_border_info();
        let mut state = FilterState::new();
        let out1 = filter_child_output(b"hello\x1b", 80, 24, &bi, &mut state);
        let out2 = filter_child_output(b"[5;10H", 80, 24, &bi, &mut state);

        let combined = [out1, out2].concat();
        assert_eq!(
            std::str::from_utf8(&combined).unwrap(),
            "hello\x1b[6;11H"
        );
    }

    #[test]
    fn test_split_sgr_color_sequence() {
        // Color sequence split: "\x1b[38;2;255" then ";128;0m"
        let bi = test_border_info();
        let mut state = FilterState::new();
        let out1 = filter_child_output(b"\x1b[38;2;255", 80, 24, &bi, &mut state);
        let out2 = filter_child_output(b";128;0m", 80, 24, &bi, &mut state);

        let combined = [out1, out2].concat();
        assert_eq!(
            std::str::from_utf8(&combined).unwrap(),
            "\x1b[38;2;255;128;0m"
        );
    }

    #[test]
    fn test_split_text_between_sequences() {
        // Normal text followed by a split sequence
        let bi = test_border_info();
        let mut state = FilterState::new();
        let out1 = filter_child_output(b"abc\x1b[", 80, 24, &bi, &mut state);
        let out2 = filter_child_output(b"1;1Hxyz", 80, 24, &bi, &mut state);

        let combined = [out1, out2].concat();
        assert_eq!(
            std::str::from_utf8(&combined).unwrap(),
            "abc\x1b[2;2Hxyz"
        );
    }

    // === Bug fix tests ===

    #[test]
    fn test_cr_converts_to_cha2() {
        // BUG 1: \r should become \x1b[2G (CHA column 2) instead of going to column 1
        let input = b"\r";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2G");
    }

    #[test]
    fn test_newline_with_cr() {
        // \r should become CHA(2), \n should pass through + border repair
        let input = b"\r\n";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Starts with CR->CHA(2) then LF
        assert!(s.starts_with("\x1b[2G\n"));
        // LF triggers border repair
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[23;1H"));
        assert!(s.contains("\x1b[23;80H"));
        assert!(s.contains("\x1b8"));
    }

    #[test]
    fn test_csi_e_cnl_converts_to_b_plus_cha() {
        // BUG 3: CSI 3E (Cursor Next Line) should become CSI 3B + CHA(2)
        let input = b"\x1b[3E";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[3B\x1b[2G");
    }

    #[test]
    fn test_csi_f_cpl_converts_to_a_plus_cha() {
        // BUG 3: CSI 2F (Cursor Previous Line) should become CSI 2A + CHA(2)
        let input = b"\x1b[2F";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2A\x1b[2G");
    }

    #[test]
    fn test_el_0k_redraws_right_border() {
        // BUG 2: CSI K (erase to end of line) should pass through and redraw right border
        let input = b"\x1b[K";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should contain the EL sequence
        assert!(s.starts_with("\x1b[K"));
        // Should contain save cursor, move to column 80, draw border char, reset, restore cursor
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[80G"));
        assert!(s.contains('│'));
        assert!(s.contains("\x1b[0m"));
        assert!(s.contains("\x1b8"));
    }

    #[test]
    fn test_el_1k_redraws_left_border() {
        // BUG 4: CSI 1K (erase from start of line) should pass through and redraw left border
        let input = b"\x1b[1K";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should contain the EL 1K sequence
        assert!(s.starts_with("\x1b[1K"));
        // Should contain save cursor, move to column 1, draw border char, reset, restore cursor
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[1G"));
        assert!(s.contains('│'));
        assert!(s.contains("\x1b[0m"));
        assert!(s.contains("\x1b8"));
    }

    #[test]
    fn test_el_2k_redraws_both_borders() {
        // CSI 2K (erase entire line) should redraw both borders
        let input = b"\x1b[2K";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[2K"));
        // Should redraw left border at column 1
        assert!(s.contains("\x1b[1G"));
        // Should redraw right border at column 80
        assert!(s.contains("\x1b[80G"));
        // Should contain two border chars
        assert_eq!(s.matches('│').count(), 2);
    }

    // === New bug fix tests ===

    #[test]
    fn test_ed_0j_converts_to_ech_rows() {
        // ED 0J (erase from cursor to end) should be converted to row-by-row ECH
        // with cursor save/restore, instead of passing through raw CSI J
        let input = b"\x1b[J"; // ED 0J (default)
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should save cursor
        assert!(s.starts_with("\x1b7"));
        // Should contain ECH for inner rows (rows 2..=23 for 24-row outer)
        assert!(s.contains("\x1b[2;2H\x1b[78X"));
        assert!(s.contains("\x1b[23;2H\x1b[78X"));
        // Should restore cursor
        assert!(s.ends_with("\x1b8"));
        // Should NOT contain raw ED sequence
        assert!(!s.contains("\x1b[J"));
        assert!(!s.contains("\x1b[0J"));
    }

    #[test]
    fn test_ed_0j_explicit_param() {
        // ED with explicit 0 parameter
        let input = b"\x1b[0J";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b7"));
        assert!(s.contains("\x1b[2;2H\x1b[78X"));
        assert!(s.ends_with("\x1b8"));
    }

    #[test]
    fn test_ed_1j_converts_to_ech_rows() {
        // ED 1J (erase from beginning to cursor) should also be converted
        let input = b"\x1b[1J";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b7"));
        assert!(s.contains("\x1b[2;2H\x1b[78X"));
        assert!(s.contains("\x1b[23;2H\x1b[78X"));
        assert!(s.ends_with("\x1b8"));
    }

    #[test]
    fn test_hpa_offset() {
        // HPA (CSI col `) should get same offset as CHA (CSI col G)
        let input = b"\x1b[10`";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[11`");
    }

    #[test]
    fn test_hpa_default_param() {
        // HPA with no param defaults to column 1
        let input = b"\x1b[`";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2`");
    }

    #[test]
    fn test_hpa_clamped() {
        // HPA beyond inner width should be clamped
        let input = b"\x1b[200`";
        let output = filter(input, 80, 24, &mut FilterState::new());
        // inner_width = 78, so clamped to 78+1=79
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[79`");
    }

    #[test]
    fn test_lf_repairs_bottom_border() {
        // LF should pass through and append bottom-row border repair
        let input = b"\n";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should start with the LF itself
        assert!(s.starts_with('\n'));
        // Should save cursor
        assert!(s.contains("\x1b7"));
        // Should draw left border at bottom row (row 23), col 1
        assert!(s.contains("\x1b[23;1H"));
        // Should draw right border at bottom row, col 80
        assert!(s.contains("\x1b[23;80H"));
        // Should contain two border chars
        assert_eq!(s.matches('│').count(), 2);
        // Should restore cursor
        assert!(s.contains("\x1b8"));
    }

    #[test]
    fn test_lf_in_text_stream() {
        // Multiple LFs in text should each get border repair
        let input = b"a\nb\n";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should have 2 LFs and thus 4 border chars (2 per LF)
        assert_eq!(s.matches('│').count(), 4);
    }
}
