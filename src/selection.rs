use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardSelection {
    Clipboard,
    Primary,
}

impl ClipboardSelection {
    pub const ALL: [Self; 2] = [Self::Clipboard, Self::Primary];

    pub fn spice_id(self) -> u8 {
        match self {
            Self::Clipboard => 0,
            Self::Primary => 1,
        }
    }

    pub fn from_spice_id(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Clipboard),
            1 => Some(Self::Primary),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Clipboard => "clipboard",
            Self::Primary => "primary selection",
        }
    }
}

impl fmt::Display for ClipboardSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}
