//! One size for the whole window: text, icons and the fixed pixel sizes in
//! the stylesheet grow together (Settings → Appearance → Size).
//!
//! libadwaita is drawn for an 11 pt desktop font; KDE's default is 10 pt,
//! which left everything a little small (Ethan, 2026-11-08). "Automatic"
//! brings a smaller desktop font up to 11 pt.

use std::cell::{Cell, RefCell};

/// The choices in Settings, as (label, factor); 0 = automatic.
pub const CHOICES: &[(&str, f64)] =
    &[("Automatic", 0.0), ("100%", 1.0), ("110%", 1.1), ("125%", 1.25), ("150%", 1.5), ("175%", 1.75)];

thread_local! {
    static FACTOR: Cell<f64> = const { Cell::new(1.0) };
    /// The desktop's font name, before scaling.
    static BASE_FONT: RefCell<Option<String>> = const { RefCell::new(None) };
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
}

/// The factor in use.
pub fn factor() -> f64 {
    FACTOR.with(Cell::get)
}

/// `n` pixels at the current size.
pub fn px(n: i32) -> i32 {
    (n as f64 * factor()).round() as i32
}

/// "Noto Sans,  10" → ("Noto Sans", 10.0).
fn split_font(name: &str) -> Option<(String, f64)> {
    let name = name.trim();
    let (family, size) = name.rsplit_once(|c: char| c.is_whitespace() || c == ',')?;
    let size: f64 = size.parse().ok().filter(|s: &f64| *s > 0.0)?;
    Some((family.trim().trim_end_matches(',').trim().to_string(), size))
}

/// The factor for a setting (0 = automatic) and the desktop font.
pub fn factor_for(setting: f64, desktop_font: Option<&str>) -> f64 {
    if setting > 0.0 {
        return setting.clamp(1.0, 2.0);
    }
    match desktop_font.and_then(split_font) {
        Some((_, size)) if size < 11.0 => ((11.0 / size) * 100.0).round() / 100.0,
        _ => 1.0,
    }
}

/// `css` with every size of 2 px or more multiplied by `f` (hairlines stay).
pub fn scale_css(css: &str, f: f64) -> String {
    let mut out = String::with_capacity(css.len());
    let bytes = css.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // A number not glued to a name before it (e.g. "h2px" stays).
        let starts = c.is_ascii_digit()
            && (i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'-'
                    || bytes[i - 1] == b'.'
                    || bytes[i - 1] == b'_'));
        if starts {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
                j += 1;
            }
            if css[j..].starts_with("px")
                && let Ok(v) = css[i..j].parse::<f64>()
                && v >= 2.0
            {
                let scaled = (v * f).round();
                out.push_str(&format!("{scaled}"));
                i = j;
                continue;
            }
            out.push_str(&css[i..j]);
            i = j;
            continue;
        }
        let ch = css[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

/// Icon sizes at `f` (the theme's are 16 px, 32 px for large icons;
/// GtkImage tags itself `.normal-icons` or `.large-icons`).
fn icon_css(f: f64) -> String {
    let n = (16.0 * f).round();
    let l = (32.0 * f).round();
    format!(".normal-icons {{ -gtk-icon-size: {n}px; }}\n.large-icons {{ -gtk-icon-size: {l}px; }}\n")
}

/// Load `css` (the app's stylesheet) at `setting`, replacing an earlier load.
pub fn apply(setting: f64, css: &str) {
    let Some(settings) = gtk::Settings::default() else { return };
    let base = BASE_FONT.with_borrow_mut(|b| {
        b.get_or_insert_with(|| settings.gtk_font_name().map(|s| s.to_string()).unwrap_or_default()).clone()
    });
    let f = factor_for(setting, Some(&base));
    FACTOR.with(|c| c.set(f));
    if let Some((family, size)) = split_font(&base) {
        let scaled = ((size * f) * 10.0).round() / 10.0;
        settings.set_gtk_font_name(Some(&format!("{family} {scaled}")));
    }
    let Some(display) = gtk::gdk::Display::default() else { return };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&format!("{}\n{}", scale_css(css, f), icon_css(f)));
    PROVIDER.with_borrow_mut(|p| {
        if let Some(old) = p.take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        *p = Some(provider);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_brings_small_fonts_to_eleven() {
        assert_eq!(factor_for(0.0, Some("Noto Sans,  10")), 1.1);
        assert_eq!(factor_for(0.0, Some("Cantarell 11")), 1.0);
        assert_eq!(factor_for(0.0, Some("Inter 12")), 1.0);
        assert_eq!(factor_for(0.0, None), 1.0);
        assert_eq!(factor_for(1.25, Some("Noto Sans 10")), 1.25);
        assert_eq!(split_font("Noto Sans,  10"), Some(("Noto Sans".into(), 10.0)));
    }

    #[test]
    fn sizes_grow_but_hairlines_stay() {
        let css = ".a { min-width: 40px; border: 1px solid red; padding: 6px 12px; font-size: 1.35em; }\n.h2px { margin: 0; }";
        assert_eq!(
            scale_css(css, 1.5),
            ".a { min-width: 60px; border: 1px solid red; padding: 9px 18px; font-size: 1.35em; }\n.h2px { margin: 0; }"
        );
    }
}
