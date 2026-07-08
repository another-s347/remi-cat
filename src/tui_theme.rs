use ratatui::prelude::{Color, Modifier, Style};

pub(crate) const ACCENT: Color = Color::Rgb(109, 209, 255);
pub(crate) const DIM: Color = Color::Rgb(118, 124, 134);
pub(crate) const BORDER: Color = Color::Rgb(63, 68, 78);
pub(crate) const SUCCESS: Color = Color::Rgb(122, 222, 151);

pub(crate) fn accent_style(modifier: Modifier) -> Style {
    Style::default().fg(ACCENT).add_modifier(modifier)
}

pub(crate) fn dim_style() -> Style {
    Style::default().fg(DIM)
}

pub(crate) fn border_style() -> Style {
    Style::default().fg(BORDER)
}

pub(crate) fn selection_marker(selected: bool) -> &'static str {
    if selected {
        "›"
    } else {
        " "
    }
}
