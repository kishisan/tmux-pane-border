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
enum FilterState {
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

/// Process child PTY output, offsetting absolute coordinates for the border.
/// Returns the transformed bytes to write to the outer terminal.
pub fn filter_child_output(input: &[u8], outer_width: u16, outer_height: u16) -> Vec<u8> {
    let inner_height = outer_height.saturating_sub(2);
    let inner_width = outer_width.saturating_sub(2);
    let mut output = Vec::with_capacity(input.len() + input.len() / 4);
    let mut state = FilterState::Ground;

    for &byte in input {
        match state {
            FilterState::Ground => {
                if byte == 0x1B {
                    state = FilterState::Escape;
                } else if byte == 0x9B {
                    // 8-bit CSI
                    state = FilterState::Csi { params: Vec::new() };
                } else {
                    output.push(byte);
                }
            }
            FilterState::Escape => {
                match byte {
                    b'[' => {
                        state = FilterState::Csi { params: Vec::new() };
                    }
                    b']' => {
                        // OSC sequence - pass through
                        output.extend_from_slice(b"\x1b]");
                        state = FilterState::Osc;
                    }
                    _ => {
                        // Other escape sequence (e.g., ESC M for reverse index)
                        output.push(0x1B);
                        output.push(byte);
                        state = FilterState::Ground;
                    }
                }
            }
            FilterState::Csi { ref mut params } => {
                if byte >= 0x20 && byte <= 0x3F {
                    // Parameter byte or intermediate byte
                    params.push(byte);
                } else if byte >= 0x40 && byte <= 0x7E {
                    // Final byte - process the CSI sequence
                    let transformed = transform_csi(params, byte, inner_width, inner_height);
                    output.extend_from_slice(transformed.as_bytes());
                    state = FilterState::Ground;
                } else {
                    // Unexpected byte - dump what we have
                    output.extend_from_slice(b"\x1b[");
                    output.extend_from_slice(params);
                    output.push(byte);
                    state = FilterState::Ground;
                }
            }
            FilterState::Osc => {
                if byte == 0x07 {
                    // BEL terminates OSC
                    output.push(byte);
                    state = FilterState::Ground;
                } else if byte == 0x1B {
                    // Possible start of ST (ESC \)
                    state = FilterState::OscEscape;
                } else {
                    output.push(byte);
                }
            }
            FilterState::OscEscape => {
                if byte == b'\\' {
                    // ST (ESC \) terminates OSC
                    output.push(0x1B);
                    output.push(byte);
                    state = FilterState::Ground;
                } else {
                    // Not ST, emit the ESC and continue in OSC
                    output.push(0x1B);
                    output.push(byte);
                    state = FilterState::Osc;
                }
            }
        }
    }

    // Handle incomplete sequences at end of buffer
    match state {
        FilterState::Escape => {
            output.push(0x1B);
        }
        FilterState::Csi { ref params } => {
            output.extend_from_slice(b"\x1b[");
            output.extend_from_slice(params);
        }
        FilterState::OscEscape => {
            // ESC at end of OSC - emit the pending ESC
            output.push(0x1B);
        }
        _ => {}
    }

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
fn transform_csi(params: &[u8], final_byte: u8, inner_width: u16, inner_height: u16) -> String {
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
        b'G' => {
            // CHA: CSI col G
            let col = nums.first().copied().unwrap_or(1).max(1);
            let new_col = col.min(inner_width) + 1;
            let _ = write!(result, "\x1b[{new_col}G");
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
                    // Erase from cursor to end - pass through but we'll need
                    // to redraw the border afterward (handled in main loop)
                    let _ = write!(result, "\x1b[J");
                }
                1 => {
                    // Erase from beginning to cursor
                    let _ = write!(result, "\x1b[1J");
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
            // EL (Erase in Line) - pass through, content area only
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}K");
        }
        b'A' | b'B' | b'C' | b'D' | b'E' | b'F' => {
            // Cursor movement (relative) - pass through without offset
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}{}", final_byte as char);
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

/// Transform mouse input FROM the outer terminal TO the inner PTY.
/// Removes the border offset from coordinates.
/// Returns None if the click is on the border (outside inner area).
pub fn transform_mouse_input(input: &[u8], outer_width: u16, outer_height: u16) -> Option<Vec<u8>> {
    // Try to detect SGR mouse format: ESC [ < btn ; col ; row M/m
    let s = std::str::from_utf8(input).ok()?;

    if let Some(rest) = s.strip_prefix("\x1b[<") {
        // SGR format
        let end_idx = rest.find(['M', 'm'])?;
        let final_char = rest.as_bytes()[end_idx];
        let params_str = &rest[..end_idx];
        let parts: Vec<&str> = params_str.split(';').collect();

        if parts.len() == 3 {
            let btn = parts[0];
            let col: u16 = parts[1].parse().ok()?;
            let row: u16 = parts[2].parse().ok()?;

            // Check if click is within inner area
            if col < 2 || col >= outer_width || row < 2 || row >= outer_height {
                return None; // Click on border
            }

            let inner_col = col - 1;
            let inner_row = row - 1;

            return Some(format!("\x1b[<{btn};{inner_col};{inner_row}{}", final_char as char).into_bytes());
        }
    }

    // X10 mouse: ESC [ M Cb Cx Cy (all +32 encoded)
    if input.len() == 6 && input[0] == 0x1B && input[1] == b'[' && input[2] == b'M' {
        let cb = input[3];
        let cx = input[4];
        let cy = input[5];

        let col = cx.wrapping_sub(32) as u16;
        let row = cy.wrapping_sub(32) as u16;

        if col < 2 || col >= outer_width || row < 2 || row >= outer_height {
            return None;
        }

        let inner_cx = (col - 1 + 32) as u8;
        let inner_cy = (row - 1 + 32) as u8;

        return Some(vec![0x1B, b'[', b'M', cb, inner_cx, inner_cy]);
    }

    // Not a mouse sequence, pass through as-is
    Some(input.to_vec())
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

    #[test]
    fn test_cup_offset() {
        // CSI 1;1 H should become CSI 2;2 H
        let input = b"\x1b[1;1H";
        let output = filter_child_output(input, 80, 24);
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2;2H");
    }

    #[test]
    fn test_cup_default_params() {
        // CSI H (no params) = row 1, col 1
        let input = b"\x1b[H";
        let output = filter_child_output(input, 80, 24);
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2;2H");
    }

    #[test]
    fn test_vpa_offset() {
        let input = b"\x1b[5d";
        let output = filter_child_output(input, 80, 24);
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[6d");
    }

    #[test]
    fn test_cha_offset() {
        let input = b"\x1b[10G";
        let output = filter_child_output(input, 80, 24);
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[11G");
    }

    #[test]
    fn test_decstbm_offset() {
        // CSI 1;22 r → CSI 2;23 r
        let input = b"\x1b[1;22r";
        let output = filter_child_output(input, 80, 24);
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[2;23r");
    }

    #[test]
    fn test_relative_movement_passthrough() {
        // Relative cursor movement should pass through unchanged
        let input = b"\x1b[5A";
        let output = filter_child_output(input, 80, 24);
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[5A");
    }

    #[test]
    fn test_plain_text_passthrough() {
        let input = b"Hello, world!";
        let output = filter_child_output(input, 80, 24);
        assert_eq!(&output, input);
    }

    #[test]
    fn test_sgr_passthrough() {
        // Color sequences should pass through
        let input = b"\x1b[31mred\x1b[0m";
        let output = filter_child_output(input, 80, 24);
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[31mred\x1b[0m");
    }

    #[test]
    fn test_mouse_input_transform() {
        // SGR mouse click at col 5, row 3 -> inner col 4, row 2
        let input = b"\x1b[<0;5;3M";
        let result = transform_mouse_input(input, 80, 24);
        assert!(result.is_some());
        let out = result.unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "\x1b[<0;4;2M");
    }

    #[test]
    fn test_mouse_input_on_border() {
        // Click at col 1, row 1 (top-left border) should be None
        let input = b"\x1b[<0;1;1M";
        let result = transform_mouse_input(input, 80, 24);
        assert!(result.is_none());
    }
}
