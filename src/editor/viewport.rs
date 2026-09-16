use super::EditorEffects;
use super::buffer::EditorBuffer;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Viewport {
    top_line: usize,
    left_column: usize,
}

impl Viewport {
    pub fn top_line(&self) -> usize {
        self.top_line
    }

    pub fn left_column(&self) -> usize {
        self.left_column
    }

    pub fn follow_cursor<S>(&mut self, editor: &EditorBuffer<S>, width: usize, height: usize)
    where
        S: EditorEffects,
    {
        let cursor = editor.cursor();
        if cursor.line < self.top_line {
            self.top_line = cursor.line;
        } else if height > 0 && cursor.line >= self.top_line + height {
            self.top_line = cursor.line - height + 1;
        }

        if cursor.display_column < self.left_column {
            self.left_column = cursor.display_column;
        } else if width > 0 && cursor.display_column >= self.left_column + width {
            self.left_column = cursor.display_column - width + 1;
        }
    }

    pub fn scroll_vertical(&mut self, delta: isize, line_count: usize, height: usize) {
        let max_top = line_count.saturating_sub(height);
        self.top_line = self.top_line.saturating_add_signed(delta).min(max_top);
    }

    pub fn set_top_line(&mut self, top_line: usize, line_count: usize, height: usize) {
        self.top_line = top_line.min(line_count.saturating_sub(height));
    }

    pub fn scroll_horizontal(&mut self, delta: isize) {
        self.left_column = self.left_column.saturating_add_signed(delta);
    }
}
