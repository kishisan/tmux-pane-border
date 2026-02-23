use crate::config::BorderStyle;
use std::fmt::Write;

/// Characters for each border style: (top_left, top_right, bottom_left, bottom_right, horizontal, vertical)
fn style_chars(style: BorderStyle) -> (char, char, char, char, char, char) {
    match style {
        BorderStyle::Rounded => ('╭', '╮', '╰', '╯', '─', '│'),
        BorderStyle::Heavy => ('┏', '┓', '┗', '┛', '━', '┃'),
        BorderStyle::Double => ('╔', '╗', '╚', '╝', '═', '║'),
        BorderStyle::Single => ('┌', '┐', '└', '┘', '─', '│'),
        BorderStyle::Ascii => ('+', '+', '+', '+', '-', '|'),
    }
}

/// Parse a hex color string like "#61afef" into (r, g, b).
fn parse_hex_color(color: &str) -> Option<(u8, u8, u8)> {
    let hex = color.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Generate the ANSI escape sequence to set foreground color from a hex string.
fn fg_color_seq(color: &str) -> String {
    if let Some((r, g, b)) = parse_hex_color(color) {
        format!("\x1b[38;2;{r};{g};{b}m")
    } else {
        // Fallback: default foreground
        String::new()
    }
}

/// Render the full border frame into an ANSI escape string.
/// `width` and `height` are the outer dimensions (the full pane size).
pub fn render_border(width: u16, height: u16, style: BorderStyle, color: &str) -> String {
    if width < 3 || height < 3 {
        return String::new();
    }

    let (tl, tr, bl, br, h, v) = style_chars(style);
    let color_seq = fg_color_seq(color);
    let reset = "\x1b[0m";

    let inner_width = (width - 2) as usize;

    let mut buf = String::with_capacity((width as usize + 20) * height as usize);

    // Save cursor, hide cursor
    buf.push_str("\x1b[?25l");

    // Top border: move to row 1, col 1
    let _ = write!(buf, "\x1b[1;1H{color_seq}{tl}");
    for _ in 0..inner_width {
        buf.push(h);
    }
    let _ = write!(buf, "{tr}{reset}");

    // Side borders
    for row in 2..height {
        let _ = write!(buf, "\x1b[{row};1H{color_seq}{v}{reset}");
        let _ = write!(buf, "\x1b[{row};{width}H{color_seq}{v}{reset}");
    }

    // Bottom border
    let _ = write!(buf, "\x1b[{height};1H{color_seq}{bl}");
    for _ in 0..inner_width {
        buf.push(h);
    }
    let _ = write!(buf, "{br}{reset}");

    // Show cursor
    buf.push_str("\x1b[?25h");

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex_color() {
        assert_eq!(parse_hex_color("#61afef"), Some((0x61, 0xaf, 0xef)));
        assert_eq!(parse_hex_color("#000000"), Some((0, 0, 0)));
        assert_eq!(parse_hex_color("#ffffff"), Some((255, 255, 255)));
        assert_eq!(parse_hex_color("invalid"), None);
    }

    #[test]
    fn test_render_border_small() {
        // Too small should return empty
        assert!(render_border(2, 2, BorderStyle::Rounded, "#ffffff").is_empty());
    }

    #[test]
    fn test_render_border_contains_corners() {
        let output = render_border(10, 5, BorderStyle::Rounded, "#ffffff");
        assert!(output.contains('╭'));
        assert!(output.contains('╮'));
        assert!(output.contains('╰'));
        assert!(output.contains('╯'));
    }
}
