//! Pure functions for building DBus signal bodies the broker emits
//! back to embedded clients. Kept separate from the I/O / per-connection
//! pump in `mod.rs` because:
//!
//! - These have **zero** dependencies on `Connection` / `DbusBroker` —
//!   pure transformations on `(text, cursor_range)` → `Vec<(String, i32)>`.
//! - The fcitx5 format-flag bit values live in this one place so the
//!   pump call site doesn't repeat the magic numbers.
//! - Test surface is large (UTF-8 boundary edge cases, format-flag
//!   arithmetic) and benefits from being co-located with its tests.

/// Per fcitx5's `FcitxTextFormatFlag` (fcitx-utils/textformatflags.h):
/// `Underline = 1 << 3 = 8`. Marks the preedit text as IM-tentative —
/// without it, GTK fcitx-gtk renders preedit as plain content with no
/// visual distinction.
pub(super) const UNDERLINE: i32 = 1 << 3;
/// Per fcitx5's `FcitxTextFormatFlag`: `HighLight = 1 << 4 = 16`.
/// Inverts colors on the segment to mark "currently composing".
pub(super) const HIGHLIGHT: i32 = 1 << 4;

/// Split a preedit text into chunks for `UpdateFormattedPreedit` so
/// the active segment (the byte range `[begin, end)` reported by
/// winit) carries `HighLight` on top of `Underline`. Falls back to a
/// single underlined chunk when there's no range, the range is empty,
/// or the range straddles a non-UTF-8 char boundary (defensive — winit
/// always reports valid byte offsets, but `&text[..b]` would panic
/// otherwise).
pub(super) fn build_preedit_chunks(
    text: &str,
    cursor: Option<(i32, i32)>,
    underline: i32,
    highlight: i32,
) -> Vec<(String, i32)> {
    let plain = || vec![(text.to_string(), underline)];
    let Some((begin, end)) = cursor else {
        return plain();
    };
    if begin < 0 || end <= begin {
        return plain();
    }
    let (b, e) = (begin as usize, end as usize);
    let len = text.len();
    if b > len || e > len || !text.is_char_boundary(b) || !text.is_char_boundary(e) {
        return plain();
    }
    let mut v = Vec::with_capacity(3);
    if b > 0 {
        v.push((text[..b].to_string(), underline));
    }
    v.push((text[b..e].to_string(), underline | highlight));
    if e < len {
        v.push((text[e..].to_string(), underline));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::{build_preedit_chunks, HIGHLIGHT as H, UNDERLINE as U};

    #[test]
    fn preedit_no_cursor_is_single_underline_chunk() {
        let v = build_preedit_chunks("nihao", None, U, H);
        assert_eq!(v, vec![("nihao".to_string(), U)]);
    }

    #[test]
    fn preedit_empty_range_falls_back_to_underline() {
        let v = build_preedit_chunks("nihao", Some((2, 2)), U, H);
        assert_eq!(v, vec![("nihao".to_string(), U)]);
    }

    #[test]
    fn preedit_full_range_highlights_whole_text() {
        let v = build_preedit_chunks("nihao", Some((0, 5)), U, H);
        assert_eq!(v, vec![("nihao".to_string(), U | H)]);
    }

    #[test]
    fn preedit_middle_range_splits_three_chunks() {
        let v = build_preedit_chunks("nihaonihao", Some((2, 5)), U, H);
        assert_eq!(
            v,
            vec![
                ("ni".to_string(), U),
                ("hao".to_string(), U | H),
                ("nihao".to_string(), U),
            ]
        );
    }

    #[test]
    fn preedit_range_at_start_emits_two_chunks() {
        let v = build_preedit_chunks("nihao", Some((0, 2)), U, H);
        assert_eq!(v, vec![("ni".to_string(), U | H), ("hao".to_string(), U)]);
    }

    #[test]
    fn preedit_invalid_char_boundary_falls_back() {
        // "你" is 3 bytes UTF-8. Range (1, 2) splits inside the char.
        let v = build_preedit_chunks("你好", Some((1, 2)), U, H);
        assert_eq!(v, vec![("你好".to_string(), U)]);
    }

    #[test]
    fn preedit_negative_begin_falls_back() {
        let v = build_preedit_chunks("nihao", Some((-1, 3)), U, H);
        assert_eq!(v, vec![("nihao".to_string(), U)]);
    }
}
