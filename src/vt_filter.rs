use std::fmt::Write;

/// Filters VT sequences from child output, applying coordinate offsets.
///
/// This module parses output byte-by-byte using a simple state machine to identify
/// CSI sequences that contain absolute coordinates, and offsets them by +1 to account
/// for the border frame.
///
/// We use a hand-rolled parser instead of the `vte` crate for the output filter because
/// we need to transform sequences in-place while forwarding all other bytes verbatim.

/// Parser state for tracking position within a VT/CSI sequence.
///
/// This must be persisted across calls to `filter_child_output` so that
/// sequences split across read boundaries are handled correctly.
enum ParserState {
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
    /// Inside a DCS sequence (ESC P) - pass through until ST
    Dcs,
    /// Inside a DCS sequence and just saw ESC - waiting for '\' to complete ST
    DcsEscape,
}

/// Persistent filter state across calls to `filter_child_output`.
///
/// Tracks parser state, cursor position, and flags for events that
/// require border redraws.
pub struct FilterState {
    parser: ParserState,
    /// Set when the state machine detects alt screen enter/leave,
    /// full screen clear (ED 2J/3J), or RIS (ESC c).
    /// Main loop should check and clear this after each call.
    pub needs_border_redraw: bool,
    /// Tracked cursor row in inner coordinates (1-based).
    /// Used for ED 0J/1J to only erase the correct range of rows.
    cursor_row: u16,
    /// Top of scroll region in inner coordinates (1-based). Default = 1.
    scroll_top: u16,
    /// Bottom of scroll region in inner coordinates (1-based). Default = inner_height.
    /// Stored as 0 to mean "use inner_height" (since we don't know it at construction).
    scroll_bottom: u16,
}

impl FilterState {
    pub fn new() -> Self {
        FilterState {
            parser: ParserState::Ground,
            needs_border_redraw: false,
            cursor_row: 1,
            scroll_top: 1,
            scroll_bottom: 0, // 0 means "full screen" (resolved at use site)
        }
    }

    /// Reset cursor_row and scroll region to defaults (e.g. after SIGWINCH resize).
    pub fn reset_cursor_row(&mut self) {
        self.cursor_row = 1;
        self.reset_scroll_region();
    }

    /// Reset scroll region to full screen.
    fn reset_scroll_region(&mut self) {
        self.scroll_top = 1;
        self.scroll_bottom = 0;
    }

    /// Get effective scroll bottom, resolving 0 to inner_height.
    fn effective_scroll_bottom(&self, inner_height: u16) -> u16 {
        if self.scroll_bottom == 0 { inner_height } else { self.scroll_bottom }
    }

    /// Check and clear the needs_border_redraw flag.
    pub fn take_border_redraw(&mut self) -> bool {
        let v = self.needs_border_redraw;
        self.needs_border_redraw = false;
        v
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
        match state.parser {
            ParserState::Ground => {
                if byte == 0x1B {
                    state.parser = ParserState::Escape;
                } else if byte == 0x9B {
                    // 8-bit CSI
                    state.parser = ParserState::Csi { params: Vec::new() };
                } else if byte == 0x0D {
                    // BUG 1 fix: \r would move cursor to column 1 (left border).
                    // Convert to CHA column 2 (inner left edge) instead.
                    output.extend_from_slice(b"\x1b[2G");
                } else if byte == 0x0A {
                    // LF: pass through. Only repair borders when scroll actually occurs
                    // (cursor at bottom of scroll region).
                    output.push(byte);
                    let scroll_bottom = state.effective_scroll_bottom(inner_height);
                    if state.cursor_row >= scroll_bottom {
                        // Scroll occurred — repair bottom row side borders
                        let v = border_info.vertical_char;
                        let color = border_info.color_seq;
                        let fg_reset = "\x1b[39m";
                        let repair_row = scroll_bottom + 1; // outer coordinate
                        let mut repair = String::new();
                        let _ = write!(
                            repair,
                            "\x1b[?2026h\x1b7\x1b[{repair_row};1H{color}{v}{fg_reset}\x1b[{repair_row};{outer_width}H{color}{v}{fg_reset}\x1b8\x1b[?2026l",
                        );
                        output.extend_from_slice(repair.as_bytes());
                    } else {
                        // No scroll — just move cursor down
                        state.cursor_row += 1;
                    }
                } else {
                    output.push(byte);
                }
            }
            ParserState::Escape => {
                match byte {
                    b'[' => {
                        state.parser = ParserState::Csi { params: Vec::new() };
                    }
                    b']' => {
                        // OSC sequence - pass through
                        output.extend_from_slice(b"\x1b]");
                        state.parser = ParserState::Osc;
                    }
                    b'P' => {
                        // DCS sequence - pass through
                        output.extend_from_slice(b"\x1bP");
                        state.parser = ParserState::Dcs;
                    }
                    b'M' => {
                        // Reverse Index: pass through. Only repair borders when
                        // reverse scroll actually occurs (cursor at top of scroll region).
                        output.push(0x1B);
                        output.push(byte);
                        if state.cursor_row <= state.scroll_top {
                            // Reverse scroll occurred — repair top row side borders
                            let v = border_info.vertical_char;
                            let color = border_info.color_seq;
                            let fg_reset = "\x1b[39m";
                            let repair_row = state.scroll_top + 1; // outer coordinate
                            let mut repair = String::new();
                            let _ = write!(
                                repair,
                                "\x1b[?2026h\x1b7\x1b[{repair_row};1H{color}{v}{fg_reset}\x1b[{repair_row};{outer_width}H{color}{v}{fg_reset}\x1b8\x1b[?2026l",
                            );
                            output.extend_from_slice(repair.as_bytes());
                        } else {
                            // No scroll — just move cursor up
                            state.cursor_row -= 1;
                        }
                        state.parser = ParserState::Ground;
                    }
                    b'c' => {
                        // RIS (Reset Initial State) - pass through and flag for border redraw
                        output.push(0x1B);
                        output.push(byte);
                        state.needs_border_redraw = true;
                        state.cursor_row = 1;
                        state.reset_scroll_region();
                        state.parser = ParserState::Ground;
                    }
                    _ => {
                        // Other escape sequences - pass through unchanged
                        output.push(0x1B);
                        output.push(byte);
                        state.parser = ParserState::Ground;
                    }
                }
            }
            ParserState::Csi { ref mut params } => {
                if byte >= 0x20 && byte <= 0x3F {
                    // Parameter byte or intermediate byte
                    params.push(byte);
                } else if byte >= 0x40 && byte <= 0x7E {
                    // Final byte - process the CSI sequence
                    let params_owned = std::mem::take(params);
                    let transformed = transform_csi(&params_owned, byte, inner_width, inner_height, outer_width, border_info, state);
                    output.extend_from_slice(transformed.as_bytes());
                    state.parser = ParserState::Ground;
                } else {
                    // Unexpected byte - dump what we have
                    output.extend_from_slice(b"\x1b[");
                    output.extend_from_slice(params);
                    output.push(byte);
                    state.parser = ParserState::Ground;
                }
            }
            ParserState::Osc => {
                if byte == 0x07 {
                    // BEL terminates OSC
                    output.push(byte);
                    state.parser = ParserState::Ground;
                } else if byte == 0x1B {
                    // Possible start of ST (ESC \)
                    state.parser = ParserState::OscEscape;
                } else {
                    output.push(byte);
                }
            }
            ParserState::OscEscape => {
                if byte == b'\\' {
                    // ST (ESC \) terminates OSC
                    output.push(0x1B);
                    output.push(byte);
                    state.parser = ParserState::Ground;
                } else {
                    // Not ST, emit the ESC and continue in OSC
                    output.push(0x1B);
                    output.push(byte);
                    state.parser = ParserState::Osc;
                }
            }
            ParserState::Dcs => {
                if byte == 0x1B {
                    // Possible start of ST (ESC \)
                    state.parser = ParserState::DcsEscape;
                } else {
                    output.push(byte);
                }
            }
            ParserState::DcsEscape => {
                if byte == b'\\' {
                    // ST (ESC \) terminates DCS
                    output.push(0x1B);
                    output.push(byte);
                    state.parser = ParserState::Ground;
                } else {
                    // Not ST, emit the ESC and continue in DCS
                    output.push(0x1B);
                    output.push(byte);
                    state.parser = ParserState::Dcs;
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
fn transform_csi(params: &[u8], final_byte: u8, inner_width: u16, inner_height: u16, outer_width: u16, border_info: &BorderInfo, state: &mut FilterState) -> String {
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
            let clamped_row = row.min(inner_height);
            let new_row = clamped_row + 1;
            let new_col = col.min(inner_width) + 1;
            state.cursor_row = clamped_row;
            let _ = write!(result, "\x1b[{new_row};{new_col}H");
        }
        b'd' => {
            // VPA: CSI row d
            let row = nums.first().copied().unwrap_or(1).max(1);
            let clamped_row = row.min(inner_height);
            state.cursor_row = clamped_row;
            let new_row = clamped_row + 1;
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
            state.scroll_top = top;
            state.scroll_bottom = bottom;
            let new_top = top + 1;
            let new_bottom = bottom + 1;
            let _ = write!(result, "\x1b[{new_top};{new_bottom}r");
        }
        b'J' => {
            // ED (Erase in Display) - handle carefully to protect borders
            let mode = nums.first().copied().unwrap_or(0);
            match mode {
                0 => {
                    // Erase from cursor to end: clear remainder of cursor line,
                    // then erase all rows below.
                    let start_row = state.cursor_row.max(1);
                    let v = border_info.vertical_char;
                    let color = border_info.color_seq;
                    let fg_reset = "\x1b[39m";
                    let _ = write!(result, "\x1b[?2026h"); // begin synchronized output
                    // Erase cursor line from cursor to end + repair right border
                    let _ = write!(result, "\x1b[K\x1b7\x1b[{outer_width}G{color}{v}{fg_reset}\x1b8");
                    // Erase all rows below cursor line
                    let _ = write!(result, "\x1b7"); // save cursor
                    for row in (start_row + 1 + 1)..=(inner_height + 1) {
                        let _ = write!(result, "\x1b[{row};2H\x1b[{}X", inner_width);
                    }
                    let _ = write!(result, "\x1b8"); // restore cursor
                    let _ = write!(result, "\x1b[?2026l"); // end synchronized output
                }
                1 => {
                    // Erase from beginning to cursor: erase full rows above
                    // cursor line, then erase cursor line from start to cursor.
                    let end_row = state.cursor_row.min(inner_height);
                    let v = border_info.vertical_char;
                    let color = border_info.color_seq;
                    let fg_reset = "\x1b[39m";
                    let _ = write!(result, "\x1b[?2026h"); // begin synchronized output
                    let _ = write!(result, "\x1b7"); // save cursor
                    // Erase all rows above the cursor line
                    for row in 2..=(end_row) {
                        let _ = write!(result, "\x1b[{row};2H\x1b[{}X", inner_width);
                    }
                    let _ = write!(result, "\x1b8"); // restore cursor
                    // Erase cursor line from start to cursor + repair left border
                    let _ = write!(result, "\x1b[1K\x1b7\x1b[1G{color}{v}{fg_reset}\x1b8");
                    let _ = write!(result, "\x1b[?2026l"); // end synchronized output
                }
                2 | 3 => {
                    // Erase entire display - we convert to clearing inner area only
                    // by erasing each inner line
                    let _ = write!(result, "\x1b[?2026h"); // begin synchronized output
                    for row in 2..=(inner_height + 1) {
                        let _ = write!(result, "\x1b[{row};2H\x1b[{}X", inner_width);
                    }
                    // Restore cursor to inner area top-left, matching expected ED 2J behavior
                    let _ = write!(result, "\x1b[2;2H");
                    let _ = write!(result, "\x1b[?2026l"); // end synchronized output
                    state.cursor_row = 1;
                    state.reset_scroll_region();
                    state.needs_border_redraw = true;
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
            let reset = "\x1b[39m";
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
        b'A' => {
            // CUU (Cursor Up) - pass through, track cursor row
            let count = nums.first().copied().unwrap_or(1).max(1);
            state.cursor_row = state.cursor_row.saturating_sub(count).max(1);
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}A");
        }
        b'B' => {
            // CUD (Cursor Down) - pass through, track cursor row
            let count = nums.first().copied().unwrap_or(1).max(1);
            state.cursor_row = (state.cursor_row + count).min(inner_height);
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}B");
        }
        b'C' | b'D' => {
            // CUF/CUB (Cursor Forward/Back) - pass through without offset
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}{}", final_byte as char);
        }
        b'E' => {
            // BUG 3 fix: CNL (Cursor Next Line) moves down then to column 1.
            // Convert to CUD (down) + CHA column 2 to stay inside border.
            let count = nums.first().copied().unwrap_or(1).max(1);
            state.cursor_row = (state.cursor_row + count).min(inner_height);
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}B\x1b[2G");
        }
        b'F' => {
            // BUG 3 fix: CPL (Cursor Previous Line) moves up then to column 1.
            // Convert to CUU (up) + CHA column 2 to stay inside border.
            let count = nums.first().copied().unwrap_or(1).max(1);
            state.cursor_row = state.cursor_row.saturating_sub(count).max(1);
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}A\x1b[2G");
        }
        b'h' | b'l' if is_private => {
            // Private mode set/reset (e.g., ?1049h for alternate screen, ?25h for cursor)
            let param_str = std::str::from_utf8(&params[1..]).unwrap_or("");
            if param_str == "69" {
                // Block DECLRMM (Left Right Margin Mode) - would break border rendering
            } else {
                let _ = write!(result, "\x1b[?{param_str}{}", final_byte as char);
                // Detect alt screen enter/leave: ?1049h/l or ?47h/l
                if param_str == "1049" || param_str == "1047" || param_str == "47" {
                    state.needs_border_redraw = true;
                    state.cursor_row = 1;
                    state.reset_scroll_region();
                }
            }
        }
        b'L' => {
            // IL (Insert Lines): pass through, then repair side borders in scroll region
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}L");
            let scroll_bottom = state.effective_scroll_bottom(inner_height);
            repair_side_borders_in_region(&mut result, outer_width, state.scroll_top, scroll_bottom, border_info);
        }
        b'M' => {
            // DL (Delete Lines): pass through, then repair side borders in scroll region
            // Note: SGR mouse (CSI < ...M) is handled above via early return
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}M");
            let scroll_bottom = state.effective_scroll_bottom(inner_height);
            repair_side_borders_in_region(&mut result, outer_width, state.scroll_top, scroll_bottom, border_info);
        }
        b'S' => {
            // SU (Scroll Up): pass through, then repair bottom side borders in scroll region
            let count = nums.first().copied().unwrap_or(1).max(1);
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}S");
            let scroll_bottom = state.effective_scroll_bottom(inner_height);
            repair_bottom_side_borders(&mut result, outer_width, state.scroll_top, scroll_bottom, count, border_info);
        }
        b'T' if !is_private => {
            // SD (Scroll Down): pass through, then repair top side borders in scroll region
            let count = nums.first().copied().unwrap_or(1).max(1);
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}T");
            let scroll_bottom = state.effective_scroll_bottom(inner_height);
            repair_top_side_borders(&mut result, outer_width, state.scroll_top, scroll_bottom, count, border_info);
        }
        b'@' => {
            // ICH (Insert Characters): pass through, then repair right border
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}@");
            repair_right_border_current_row(&mut result, outer_width, border_info);
        }
        b'P' => {
            // DCH (Delete Characters): pass through, then repair right border
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}P");
            repair_right_border_current_row(&mut result, outer_width, border_info);
        }
        b'X' => {
            // ECH (Erase Characters): pass through, then repair right border
            let param_str = std::str::from_utf8(params).unwrap_or("");
            let _ = write!(result, "\x1b[{param_str}X");
            repair_right_border_current_row(&mut result, outer_width, border_info);
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

/// Repair side borders within a scroll region range (for IL/DL).
/// `scroll_top` and `scroll_bottom` are in inner coordinates (1-based).
/// Redraws left and right `│` on every row in the range.
fn repair_side_borders_in_region(result: &mut String, outer_width: u16, scroll_top: u16, scroll_bottom: u16, border_info: &BorderInfo) {
    let v = border_info.vertical_char;
    let color = border_info.color_seq;
    let reset = "\x1b[39m";
    let _ = write!(result, "\x1b[?2026h"); // begin synchronized output
    let _ = write!(result, "\x1b7"); // save cursor
    let outer_top = scroll_top + 1;
    let outer_bottom = scroll_bottom + 1;
    for row in outer_top..=outer_bottom {
        let _ = write!(result, "\x1b[{row};1H{color}{v}{reset}\x1b[{row};{outer_width}H{color}{v}{reset}");
    }
    let _ = write!(result, "\x1b8"); // restore cursor
    let _ = write!(result, "\x1b[?2026l"); // end synchronized output
}

/// Repair bottom `count` rows' side borders within the scroll region (for SU / scroll up).
/// `scroll_bottom` is in inner coordinates (1-based).
fn repair_bottom_side_borders(result: &mut String, outer_width: u16, scroll_top: u16, scroll_bottom: u16, count: u16, border_info: &BorderInfo) {
    let v = border_info.vertical_char;
    let color = border_info.color_seq;
    let reset = "\x1b[39m";
    let _ = write!(result, "\x1b[?2026h");
    let _ = write!(result, "\x1b7");
    let outer_bottom = scroll_bottom + 1;
    let outer_top_limit = scroll_top + 1;
    let first = outer_bottom.saturating_sub(count.saturating_sub(1)).max(outer_top_limit);
    for row in first..=outer_bottom {
        let _ = write!(result, "\x1b[{row};1H{color}{v}{reset}\x1b[{row};{outer_width}H{color}{v}{reset}");
    }
    let _ = write!(result, "\x1b8");
    let _ = write!(result, "\x1b[?2026l");
}

/// Repair top `count` rows' side borders within the scroll region (for SD / scroll down).
/// `scroll_top` is in inner coordinates (1-based).
fn repair_top_side_borders(result: &mut String, outer_width: u16, scroll_top: u16, scroll_bottom: u16, count: u16, border_info: &BorderInfo) {
    let v = border_info.vertical_char;
    let color = border_info.color_seq;
    let reset = "\x1b[39m";
    let _ = write!(result, "\x1b[?2026h");
    let _ = write!(result, "\x1b7");
    let outer_top = scroll_top + 1;
    let outer_bottom_limit = scroll_bottom + 1;
    let last = (outer_top + count).saturating_sub(1).min(outer_bottom_limit);
    for row in outer_top..=last {
        let _ = write!(result, "\x1b[{row};1H{color}{v}{reset}\x1b[{row};{outer_width}H{color}{v}{reset}");
    }
    let _ = write!(result, "\x1b8");
    let _ = write!(result, "\x1b[?2026l");
}

/// Repair the right border on the current row (for ICH/DCH/ECH).
fn repair_right_border_current_row(result: &mut String, outer_width: u16, border_info: &BorderInfo) {
    let v = border_info.vertical_char;
    let color = border_info.color_seq;
    let reset = "\x1b[39m";
    let _ = write!(result, "\x1b7\x1b[{outer_width}G{color}{v}{reset}\x1b8");
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
        let input = b"\x1b[31mred\x1b[39m";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[31mred\x1b[39m");
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
        // \r should become CHA(2), \n should pass through
        // At cursor_row=1 (not at scroll bottom), no repair occurs
        let input = b"\r\n";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Starts with CR->CHA(2) then LF
        assert!(s.starts_with("\x1b[2G\n"));
        // No repair at cursor_row=1 (not at scroll bottom)
        assert!(!s.contains('│'));
    }

    #[test]
    fn test_newline_at_scroll_bottom_repairs() {
        // LF at scroll bottom should trigger repair
        let mut state = FilterState::new();
        let bi = test_border_info();
        // Move cursor to bottom of scroll region (inner_height = 22)
        let _ = filter_child_output(b"\x1b[22;1H", 80, 24, &bi, &mut state);
        let output = filter_child_output(b"\r\n", 80, 24, &bi, &mut state);
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[23;1H"));
        assert!(s.contains("\x1b[23;80H"));
        assert!(s.contains("\x1b8"));
        assert_eq!(s.matches('│').count(), 2);
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
        // Should contain save cursor (CSI s), move to column 80, draw border char, reset, restore cursor (CSI u)
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[80G"));
        assert!(s.contains('│'));
        assert!(s.contains("\x1b[39m"));
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
        // Should contain save cursor (CSI s), move to column 1, draw border char, reset, restore cursor (CSI u)
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[1G"));
        assert!(s.contains('│'));
        assert!(s.contains("\x1b[39m"));
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
        // ED 0J (erase from cursor to end) should:
        // 1. Clear cursor line remainder (EL 0K) + repair right border
        // 2. ECH for all rows below cursor
        let input = b"\x1b[J"; // ED 0J (default)
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should begin with synchronized output
        assert!(s.starts_with("\x1b[?2026h"));
        // Should contain EL 0K for cursor line
        assert!(s.contains("\x1b[K"));
        // Should repair right border on cursor line
        assert!(s.contains("\x1b[80G"));
        // cursor_row defaults to 1, so rows below = row 3 (inner 2+1) to row 23
        assert!(s.contains("\x1b[3;2H\x1b[78X"));
        assert!(s.contains("\x1b[23;2H\x1b[78X"));
        // Should use DECSC/DECRC
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b8"));
        assert!(s.ends_with("\x1b[?2026l"));
    }

    #[test]
    fn test_ed_0j_explicit_param() {
        // ED with explicit 0 parameter
        let input = b"\x1b[0J";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[?2026h"));
        // Should contain EL 0K for cursor line
        assert!(s.contains("\x1b[K"));
        assert!(s.contains("\x1b[3;2H\x1b[78X"));
        assert!(s.ends_with("\x1b[?2026l"));
    }

    #[test]
    fn test_ed_0j_with_cursor_position() {
        // ED 0J after moving cursor to row 10 should clear cursor line (EL 0K)
        // + ECH for rows 11..=22 (outer 12..=23)
        let mut state = FilterState::new();
        let bi = test_border_info();
        // Move cursor to row 10
        let _ = filter_child_output(b"\x1b[10;1H", 80, 24, &bi, &mut state);
        // Now ED 0J
        let output = filter_child_output(b"\x1b[J", 80, 24, &bi, &mut state);
        let s = std::str::from_utf8(&output).unwrap();
        // Should contain EL 0K for cursor line
        assert!(s.contains("\x1b[K"));
        // Should NOT erase rows 2..10 (before cursor)
        assert!(!s.contains("\x1b[2;2H"));
        assert!(!s.contains("\x1b[10;2H"));
        // Should erase from row 12 (inner row 11 + 1) to row 23
        assert!(s.contains("\x1b[12;2H\x1b[78X"));
        assert!(s.contains("\x1b[23;2H\x1b[78X"));
    }

    #[test]
    fn test_ed_1j_converts_to_ech_rows() {
        // ED 1J (erase from beginning to cursor) with default cursor_row=1
        // should: no rows above (loop 2..=1 is empty), then EL 1K + left border repair
        let input = b"\x1b[1J";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[?2026h"));
        assert!(s.contains("\x1b7"));
        // No ECH rows (cursor at row 1, no rows above)
        assert!(!s.contains("\x1b[2;2H\x1b[78X"));
        // Should contain EL 1K for cursor line
        assert!(s.contains("\x1b[1K"));
        // Should repair left border
        assert!(s.contains("\x1b[1G"));
        assert!(s.contains("\x1b8"));
        assert!(s.ends_with("\x1b[?2026l"));
    }

    #[test]
    fn test_ed_1j_with_cursor_position() {
        // ED 1J after moving cursor to row 10 should:
        // ECH for rows 1..=9 (outer 2..=10), then EL 1K + left border on cursor line
        let mut state = FilterState::new();
        let bi = test_border_info();
        // Move cursor to row 10
        let _ = filter_child_output(b"\x1b[10;1H", 80, 24, &bi, &mut state);
        // Now ED 1J
        let output = filter_child_output(b"\x1b[1J", 80, 24, &bi, &mut state);
        let s = std::str::from_utf8(&output).unwrap();
        // Should erase rows 2..=10 (inner rows 1..=9, outer rows 2..=10) with ECH
        assert!(s.contains("\x1b[2;2H\x1b[78X"));
        assert!(s.contains("\x1b[10;2H\x1b[78X"));
        // Should NOT erase row 11 with ECH (that's cursor line — uses EL 1K instead)
        assert!(!s.contains("\x1b[11;2H\x1b[78X"));
        // Should contain EL 1K for cursor line
        assert!(s.contains("\x1b[1K"));
        // Should NOT erase row 12 (inner row 11)
        assert!(!s.contains("\x1b[12;2H"));
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
    fn test_lf_no_repair_when_not_at_scroll_bottom() {
        // LF at cursor_row=1 (not at scroll bottom) should NOT repair borders
        let input = b"\n";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should start with the LF itself
        assert!(s.starts_with('\n'));
        // No border repair
        assert_eq!(s.matches('│').count(), 0);
        assert_eq!(s, "\n");
    }

    #[test]
    fn test_lf_repairs_at_scroll_bottom() {
        // LF at scroll bottom should trigger border repair
        let mut state = FilterState::new();
        let bi = test_border_info();
        // Move cursor to inner_height (22)
        let _ = filter_child_output(b"\x1b[22;1H", 80, 24, &bi, &mut state);
        let output = filter_child_output(b"\n", 80, 24, &bi, &mut state);
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with('\n'));
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[23;1H"));
        assert!(s.contains("\x1b[23;80H"));
        assert_eq!(s.matches('│').count(), 2);
        assert!(s.contains("\x1b8"));
    }

    #[test]
    fn test_lf_in_text_stream_no_repair_until_bottom() {
        // Multiple LFs in text from cursor_row=1 should not get border repair
        // (cursor moves from 1 to 3, never reaches scroll bottom 22)
        let input = b"a\nb\n";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert_eq!(s.matches('│').count(), 0);
    }

    #[test]
    fn test_reverse_index_repairs_top_border() {
        // ESC M (Reverse Index) should pass through and repair top row side borders
        let input = b"\x1bM";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should start with ESC M
        assert!(s.starts_with("\x1bM"));
        // Should save cursor with CSI s
        assert!(s.contains("\x1b7"));
        // Should draw left border at top row (row 2), col 1
        assert!(s.contains("\x1b[2;1H"));
        // Should draw right border at top row, col 80
        assert!(s.contains("\x1b[2;80H"));
        // Should contain two border chars
        assert_eq!(s.matches('│').count(), 2);
        // Should restore cursor with CSI u
        assert!(s.contains("\x1b8"));
    }

    // === IL/DL/SU/SD/ICH/DCH/ECH/DECLRMM tests ===

    #[test]
    fn test_il_repairs_all_side_borders() {
        // IL (CSI 3L) should pass through and repair all side borders
        let input = b"\x1b[3L";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should start with the IL sequence
        assert!(s.starts_with("\x1b[3L"));
        // Should use synchronized output and CSI s/u
        assert!(s.contains("\x1b[?2026h"));
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b8"));
        assert!(s.contains("\x1b[?2026l"));
        // Should repair all 22 inner rows (rows 2..=23), 2 borders each = 44
        assert_eq!(s.matches('│').count(), 44);
        // Check first and last inner rows
        assert!(s.contains("\x1b[2;1H"));
        assert!(s.contains("\x1b[2;80H"));
        assert!(s.contains("\x1b[23;1H"));
        assert!(s.contains("\x1b[23;80H"));
    }

    #[test]
    fn test_dl_repairs_all_side_borders() {
        // DL (CSI 2M) should pass through and repair all side borders
        let input = b"\x1b[2M";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[2M"));
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b8"));
        assert_eq!(s.matches('│').count(), 44);
    }

    #[test]
    fn test_dl_default_param() {
        // DL with no param (CSI M) should default to 1 and still repair
        let input = b"\x1b[M";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[M"));
        assert_eq!(s.matches('│').count(), 44);
    }

    #[test]
    fn test_su_repairs_bottom_borders() {
        // SU (CSI 3S) should pass through and repair bottom 3 rows
        let input = b"\x1b[3S";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[3S"));
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b8"));
        // 3 rows × 2 borders = 6
        assert_eq!(s.matches('│').count(), 6);
        // Should repair rows 21, 22, 23 (bottom 3 of inner area)
        assert!(s.contains("\x1b[21;1H"));
        assert!(s.contains("\x1b[23;80H"));
    }

    #[test]
    fn test_su_default_param() {
        // SU with no param defaults to 1
        let input = b"\x1b[S";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[S"));
        // 1 row × 2 borders = 2
        assert_eq!(s.matches('│').count(), 2);
        // Should repair bottom row (row 23)
        assert!(s.contains("\x1b[23;1H"));
        assert!(s.contains("\x1b[23;80H"));
    }

    #[test]
    fn test_sd_repairs_top_borders() {
        // SD (CSI 3T) should pass through and repair top 3 rows
        let input = b"\x1b[3T";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[3T"));
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b8"));
        // 3 rows × 2 borders = 6
        assert_eq!(s.matches('│').count(), 6);
        // Should repair rows 2, 3, 4 (top 3 of inner area)
        assert!(s.contains("\x1b[2;1H"));
        assert!(s.contains("\x1b[4;80H"));
    }

    #[test]
    fn test_sd_default_param() {
        // SD with no param defaults to 1
        let input = b"\x1b[T";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[T"));
        // 1 row × 2 borders = 2
        assert_eq!(s.matches('│').count(), 2);
        // Should repair top row (row 2)
        assert!(s.contains("\x1b[2;1H"));
        assert!(s.contains("\x1b[2;80H"));
    }

    #[test]
    fn test_ich_repairs_right_border() {
        // ICH (CSI 5@) should pass through and repair right border
        let input = b"\x1b[5@";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[5@"));
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[80G"));
        assert_eq!(s.matches('│').count(), 1);
        assert!(s.contains("\x1b8"));
    }

    #[test]
    fn test_dch_repairs_right_border() {
        // DCH (CSI 5P) should pass through and repair right border
        let input = b"\x1b[5P";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[5P"));
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[80G"));
        assert_eq!(s.matches('│').count(), 1);
        assert!(s.contains("\x1b8"));
    }

    #[test]
    fn test_ech_repairs_right_border() {
        // ECH (CSI 20X) should pass through and repair right border
        let input = b"\x1b[20X";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[20X"));
        assert!(s.contains("\x1b7"));
        assert!(s.contains("\x1b[80G"));
        assert_eq!(s.matches('│').count(), 1);
        assert!(s.contains("\x1b8"));
    }

    #[test]
    fn test_declrmm_blocked() {
        // CSI ?69h (DECLRMM enable) should be silently dropped
        let input = b"\x1b[?69h";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(output, b"");
    }

    #[test]
    fn test_declrmm_disable_blocked() {
        // CSI ?69l (DECLRMM disable) should also be silently dropped
        let input = b"\x1b[?69l";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(output, b"");
    }

    #[test]
    fn test_other_private_modes_pass_through() {
        // Other private modes like ?25h (show cursor) should still pass through
        let input = b"\x1b[?25h";
        let output = filter(input, 80, 24, &mut FilterState::new());
        assert_eq!(std::str::from_utf8(&output).unwrap(), "\x1b[?25h");
    }

    #[test]
    fn test_private_sd_passes_through() {
        // CSI ? ... T (private T) should pass through unchanged, not be treated as SD
        let input = b"\x1b[?1T";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert_eq!(s, "\x1b[?1T");
        // Should NOT contain border repair
        assert!(!s.contains('│'));
    }

    // === Alt screen / RIS / ED detection in state machine ===

    #[test]
    fn test_alt_screen_enter_sets_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[?1049h", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
        // Flag should be cleared after take
        assert!(!state.take_border_redraw());
    }

    #[test]
    fn test_alt_screen_leave_sets_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[?1049l", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_alt_screen_47_sets_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[?47h", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_alt_screen_1047_sets_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[?1047h", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_alt_screen_1047_leave_sets_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[?1047l", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_alt_screen_split_across_buffers() {
        // Alt screen sequence split across two reads should still be detected
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[?1049", 80, 24, &bi, &mut state);
        assert!(!state.needs_border_redraw); // not yet complete
        let _ = filter_child_output(b"h", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_ris_sets_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1bc", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_ris_split_across_buffers() {
        // ESC c split across two reads
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b", 80, 24, &bi, &mut state);
        assert!(!state.needs_border_redraw);
        let _ = filter_child_output(b"c", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_ed_2j_sets_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[2J", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_ed_3j_sets_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[3J", 80, 24, &bi, &mut state);
        assert!(state.take_border_redraw());
    }

    #[test]
    fn test_normal_output_no_redraw_flag() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"hello world\x1b[5;10H", 80, 24, &bi, &mut state);
        assert!(!state.needs_border_redraw);
    }

    // === DCS passthrough tests ===

    #[test]
    fn test_dcs_sequence_passthrough() {
        // DCS sequence (ESC P ... ESC \) should pass through without
        // interpreting inner bytes as CSI sequences
        let input = b"\x1bPtest\x1b[1;1Hdata\x1b\\";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        // Should contain the DCS content verbatim (CSI inside should NOT be transformed)
        assert!(s.contains("\x1bPtest\x1b[1;1Hdata\x1b\\"));
    }

    #[test]
    fn test_dcs_split_across_buffers() {
        // DCS split across reads should not corrupt state
        let mut state = FilterState::new();
        let bi = test_border_info();
        let out1 = filter_child_output(b"\x1bPsome", 80, 24, &bi, &mut state);
        let out2 = filter_child_output(b"\x1b[1;1H", 80, 24, &bi, &mut state); // should NOT be treated as CUP
        let out3 = filter_child_output(b"\x1b\\", 80, 24, &bi, &mut state);
        let combined = [out1, out2, out3].concat();
        let s = std::str::from_utf8(&combined).unwrap();
        // The CSI inside DCS should be passed through literally
        assert!(s.contains("\x1b[1;1H"));
        // Should NOT contain the transformed \x1b[2;2H
        assert!(!s.contains("\x1b[2;2H"));
    }

    // === Cursor row tracking tests ===

    #[test]
    fn test_cursor_row_tracks_cup() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        assert_eq!(state.cursor_row, 1);
        let _ = filter_child_output(b"\x1b[5;1H", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 5);
    }

    #[test]
    fn test_cursor_row_tracks_vpa() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[10d", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 10);
    }

    #[test]
    fn test_cursor_row_tracks_cuu_cud() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        // Start at row 10
        let _ = filter_child_output(b"\x1b[10;1H", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 10);
        // Move up 3
        let _ = filter_child_output(b"\x1b[3A", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 7);
        // Move down 5
        let _ = filter_child_output(b"\x1b[5B", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 12);
    }

    #[test]
    fn test_cursor_row_tracks_lf() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        assert_eq!(state.cursor_row, 1);
        let _ = filter_child_output(b"\n\n\n", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 4);
    }

    #[test]
    fn test_cursor_row_clamps_at_bottom() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        // inner_height = 22, move to row 22
        let _ = filter_child_output(b"\x1b[22;1H", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 22);
        // LF should not go beyond inner_height
        let _ = filter_child_output(b"\n", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 22);
    }

    #[test]
    fn test_cursor_row_clamps_at_top() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        assert_eq!(state.cursor_row, 1);
        // CUU should not go below 1
        let _ = filter_child_output(b"\x1b[5A", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 1);
    }

    #[test]
    fn test_cursor_row_resets_on_ed2j() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[10;1H", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 10);
        let _ = filter_child_output(b"\x1b[2J", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 1);
    }

    #[test]
    fn test_cursor_row_resets_on_alt_screen() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[10;1H", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 10);
        let _ = filter_child_output(b"\x1b[?1049h", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 1);
    }

    #[test]
    fn test_cursor_row_resets_on_ris() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[10;1H", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 10);
        let _ = filter_child_output(b"\x1bc", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 1);
    }

    // === SU/SD large count clamp tests ===

    #[test]
    fn test_su_huge_count_does_not_corrupt_top_border() {
        // CSI 9999S should repair all inner rows but NOT touch row 1 (top border)
        let input = b"\x1b[9999S";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[9999S"));
        // Should repair all 22 inner rows (rows 2..=23), clamped by .max(2)
        assert_eq!(s.matches('│').count(), 44);
        // Must NOT write to row 1 (top border)
        assert!(!s.contains("\x1b[1;1H"));
        assert!(!s.contains("\x1b[1;80H"));
        // First repaired row should be row 2
        assert!(s.contains("\x1b[2;1H"));
    }

    #[test]
    fn test_su_count_equals_inner_height() {
        // count == inner_height (22) should repair all inner rows
        let input = b"\x1b[22S";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert_eq!(s.matches('│').count(), 44);
        assert!(s.contains("\x1b[2;1H"));
        assert!(s.contains("\x1b[23;80H"));
    }

    #[test]
    fn test_sd_huge_count_does_not_corrupt_bottom_border() {
        // CSI 9999T should repair all inner rows but NOT touch bottom border row
        let input = b"\x1b[9999T";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert!(s.starts_with("\x1b[9999T"));
        // Should repair all 22 inner rows (rows 2..=23), clamped by .min(inner_height+1)
        assert_eq!(s.matches('│').count(), 44);
        // Must NOT write to row 24 (bottom border)
        assert!(!s.contains("\x1b[24;1H"));
        assert!(!s.contains("\x1b[24;80H"));
        // Last repaired row should be row 23
        assert!(s.contains("\x1b[23;1H"));
        assert!(s.contains("\x1b[23;80H"));
    }

    #[test]
    fn test_sd_count_equals_inner_height() {
        // count == inner_height (22) should repair all inner rows
        let input = b"\x1b[22T";
        let output = filter(input, 80, 24, &mut FilterState::new());
        let s = std::str::from_utf8(&output).unwrap();
        assert_eq!(s.matches('│').count(), 44);
        assert!(s.contains("\x1b[2;1H"));
        assert!(s.contains("\x1b[23;80H"));
    }

    // === Scroll region-aware repair tests (B3/B4) ===

    #[test]
    fn test_il_with_scroll_region_only_repairs_region() {
        // Set scroll region to rows 5..=15 (inner), then IL should only repair that region
        let mut state = FilterState::new();
        let bi = test_border_info();
        // Set scroll region: CSI 5;15 r
        let _ = filter_child_output(b"\x1b[5;15r", 80, 24, &bi, &mut state);
        // IL
        let output = filter_child_output(b"\x1b[3L", 80, 24, &bi, &mut state);
        let s = std::str::from_utf8(&output).unwrap();
        // Should repair 11 rows (5..=15 inner -> 6..=16 outer), 2 borders each = 22
        assert_eq!(s.matches('│').count(), 22);
        // Should repair row 6 (first in region)
        assert!(s.contains("\x1b[6;1H"));
        // Should repair row 16 (last in region)
        assert!(s.contains("\x1b[16;80H"));
        // Should NOT repair row 2 (outside region)
        assert!(!s.contains("\x1b[2;1H"));
        // Should NOT repair row 23 (outside region)
        assert!(!s.contains("\x1b[23;1H"));
    }

    #[test]
    fn test_dl_with_scroll_region_only_repairs_region() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[5;15r", 80, 24, &bi, &mut state);
        let output = filter_child_output(b"\x1b[2M", 80, 24, &bi, &mut state);
        let s = std::str::from_utf8(&output).unwrap();
        assert_eq!(s.matches('│').count(), 22);
        assert!(s.contains("\x1b[6;1H"));
        assert!(s.contains("\x1b[16;80H"));
        assert!(!s.contains("\x1b[2;1H"));
    }

    #[test]
    fn test_su_with_scroll_region_only_repairs_region_bottom() {
        // Set scroll region to rows 5..=15, SU 2 should repair bottom 2 rows of region
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[5;15r", 80, 24, &bi, &mut state);
        let output = filter_child_output(b"\x1b[2S", 80, 24, &bi, &mut state);
        let s = std::str::from_utf8(&output).unwrap();
        // 2 rows × 2 borders = 4
        assert_eq!(s.matches('│').count(), 4);
        // Should repair rows 15 and 16 (outer) = inner 14 and 15
        assert!(s.contains("\x1b[15;1H"));
        assert!(s.contains("\x1b[16;80H"));
        // Should NOT repair row 23 (outside region)
        assert!(!s.contains("\x1b[23;1H"));
    }

    #[test]
    fn test_sd_with_scroll_region_only_repairs_region_top() {
        // Set scroll region to rows 5..=15, SD 2 should repair top 2 rows of region
        let mut state = FilterState::new();
        let bi = test_border_info();
        let _ = filter_child_output(b"\x1b[5;15r", 80, 24, &bi, &mut state);
        let output = filter_child_output(b"\x1b[2T", 80, 24, &bi, &mut state);
        let s = std::str::from_utf8(&output).unwrap();
        // 2 rows × 2 borders = 4
        assert_eq!(s.matches('│').count(), 4);
        // Should repair rows 6 and 7 (outer) = inner 5 and 6
        assert!(s.contains("\x1b[6;1H"));
        assert!(s.contains("\x1b[7;80H"));
        // Should NOT repair row 2 (outside region)
        assert!(!s.contains("\x1b[2;1H"));
    }

    // === WINCH cursor_row reset test ===

    #[test]
    fn test_reset_cursor_row() {
        let mut state = FilterState::new();
        let bi = test_border_info();
        // Move cursor to row 20
        let _ = filter_child_output(b"\x1b[20;1H", 80, 24, &bi, &mut state);
        assert_eq!(state.cursor_row, 20);
        // Simulate WINCH: reset cursor_row
        state.reset_cursor_row();
        assert_eq!(state.cursor_row, 1);
    }
}
