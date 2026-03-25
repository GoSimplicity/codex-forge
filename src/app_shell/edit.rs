use super::*;

pub(super) fn cursor_row_col(text: &str, cursor: usize) -> (usize, usize) {
    let lines = line_spans(text);
    for (line_index, (start, len)) in lines.iter().enumerate() {
        if cursor <= start + len {
            return (line_index, cursor.saturating_sub(*start));
        }
    }
    let last_line = lines.len().saturating_sub(1);
    let (start, len) = lines[last_line];
    (last_line, cursor.saturating_sub(start).min(len))
}

pub(super) fn wrapped_cursor_row_col(
    text: &str,
    cursor: usize,
    max_width: usize,
) -> (usize, usize) {
    if max_width == 0 {
        return (0, 0);
    }

    let mut row = 0usize;
    let mut col = 0usize;

    for ch in text.chars().take(cursor) {
        if ch == '\n' {
            row += 1;
            col = 0;
            continue;
        }

        let ch_width = UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1)
            .min(max_width);

        if col + ch_width > max_width {
            row += 1;
            col = 0;
        }

        col += ch_width;
        if col >= max_width {
            row += col / max_width;
            col %= max_width;
        }
    }

    (row, col)
}

pub(super) fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for line in text.split('\n') {
        let len = line.chars().count();
        spans.push((start, len));
        start += len + 1;
    }
    if spans.is_empty() {
        spans.push((0, 0));
    }
    spans
}

pub(super) fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or_else(|| text.len())
}

pub(super) fn insert_char_at_cursor(edit: &mut EditState, ch: char) {
    let byte_index = char_to_byte_index(&edit.buffer, edit.cursor);
    edit.buffer.insert(byte_index, ch);
    edit.cursor += 1;
    edit.preferred_column = None;
}

pub(super) fn backspace_at_cursor(edit: &mut EditState) {
    if edit.cursor == 0 {
        return;
    }
    let start = char_to_byte_index(&edit.buffer, edit.cursor - 1);
    let end = char_to_byte_index(&edit.buffer, edit.cursor);
    edit.buffer.replace_range(start..end, "");
    edit.cursor -= 1;
    edit.preferred_column = None;
}

pub(super) fn delete_at_cursor(edit: &mut EditState) {
    let total = edit.buffer.chars().count();
    if edit.cursor >= total {
        return;
    }
    let start = char_to_byte_index(&edit.buffer, edit.cursor);
    let end = char_to_byte_index(&edit.buffer, edit.cursor + 1);
    edit.buffer.replace_range(start..end, "");
    edit.preferred_column = None;
}

pub(super) fn cycle_edit_history(edit: &mut EditState, forward: bool) {
    if edit.history_entries.is_empty() {
        return;
    }
    let current = edit.history_index.unwrap_or_else(|| {
        if forward {
            edit.history_entries.len().saturating_sub(1)
        } else {
            0
        }
    });
    let next = cycle_index(current, edit.history_entries.len(), forward);
    edit.history_index = Some(next);
    edit.buffer = edit.history_entries[next].clone();
    edit.cursor = edit.buffer.chars().count();
    edit.preferred_column = None;
}

pub(super) fn move_cursor_horizontal(edit: &mut EditState, forward: bool) {
    let total = edit.buffer.chars().count();
    if forward {
        edit.cursor = (edit.cursor + 1).min(total);
    } else {
        edit.cursor = edit.cursor.saturating_sub(1);
    }
    edit.preferred_column = None;
}

pub(super) fn move_cursor_line_edge(edit: &mut EditState, end: bool) {
    let lines = line_spans(&edit.buffer);
    for (start, len) in lines {
        if edit.cursor <= start + len {
            edit.cursor = if end { start + len } else { start };
            edit.preferred_column = None;
            return;
        }
    }
}

pub(super) fn move_cursor_vertical(edit: &mut EditState, forward: bool) {
    let lines = line_spans(&edit.buffer);
    let (current_row, current_col) = cursor_row_col(&edit.buffer, edit.cursor);
    let target_row = if forward {
        (current_row + 1).min(lines.len().saturating_sub(1))
    } else {
        current_row.saturating_sub(1)
    };
    let desired_col = edit.preferred_column.unwrap_or(current_col);
    let (target_start, target_len) = lines[target_row];
    edit.cursor = target_start + desired_col.min(target_len);
    edit.preferred_column = Some(desired_col);
}
