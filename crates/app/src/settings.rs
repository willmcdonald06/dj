//! User preferences. Set from the command line for now; the settings screen
//! (Milestone 10) will edit and save them.

use dj_analysis::KeyNotation;
use dj_ui::WaveformStyle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Settings {
    /// How keys are written: Open Key (default), Camelot or musical names.
    pub key_notation: KeyNotation,
    /// How waveforms are coloured.
    pub waveform: WaveformStyle,
}

/// Parse a `--waveform` value.
pub fn parse_waveform_style(value: &str) -> Option<WaveformStyle> {
    match value.to_ascii_lowercase().as_str() {
        "spectrum" | "rgb" => Some(WaveformStyle::Spectrum),
        "bands" | "3band" | "three-band" => Some(WaveformStyle::ThreeBand),
        _ => None,
    }
}

/// Parse a `--key-notation` value.
pub fn parse_key_notation(value: &str) -> Option<KeyNotation> {
    match value.to_ascii_lowercase().as_str() {
        "open" | "openkey" | "open-key" => Some(KeyNotation::OpenKey),
        "camelot" => Some(KeyNotation::Camelot),
        "musical" | "music" | "classic" => Some(KeyNotation::Musical),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_notation_names() {
        assert_eq!(parse_key_notation("open"), Some(KeyNotation::OpenKey));
        assert_eq!(parse_key_notation("Camelot"), Some(KeyNotation::Camelot));
        assert_eq!(parse_key_notation("musical"), Some(KeyNotation::Musical));
        assert_eq!(parse_key_notation("nope"), None);
        assert_eq!(Settings::default().key_notation, KeyNotation::OpenKey);
    }

    #[test]
    fn waveform_style_names() {
        assert_eq!(
            parse_waveform_style("bands"),
            Some(WaveformStyle::ThreeBand)
        );
        assert_eq!(parse_waveform_style("RGB"), Some(WaveformStyle::Spectrum));
        assert_eq!(parse_waveform_style("nope"), None);
        assert_eq!(Settings::default().waveform, WaveformStyle::Spectrum);
    }
}
