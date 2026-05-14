use ratatui::style::Color;
use ratatui::widgets::{Block, Borders};

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub ok: Color,
    pub warn: Color,
    pub error: Color,
    pub muted: Color,
    pub overlay_bg: Color,
    pub overlay_fg: Color,
    pub overlay_key: Color,
}

pub const DEFAULT: Theme = Theme {
    ok:         Color::Rgb(97,  175, 74),
    warn:       Color::Rgb(229, 192, 123),
    error:      Color::Rgb(224, 108, 117),
    muted:      Color::Rgb(92,  99,  112),
    overlay_bg: Color::Rgb(40,  44,  52),
    overlay_fg: Color::Rgb(171, 178, 191),
    overlay_key: Color::Rgb(97, 175, 74),
};

pub const DRACULA: Theme = Theme {
    ok:         Color::Rgb(80,  250, 123),
    warn:       Color::Rgb(241, 250, 140),
    error:      Color::Rgb(255, 85,  85),
    muted:      Color::Rgb(98,  114, 164),
    overlay_bg: Color::Rgb(40,  42,  54),
    overlay_fg: Color::Rgb(248, 248, 242),
    overlay_key: Color::Rgb(189, 147, 249),
};

pub const NORD: Theme = Theme {
    ok:         Color::Rgb(163, 190, 140),
    warn:       Color::Rgb(235, 203, 139),
    error:      Color::Rgb(191, 97,  106),
    muted:      Color::Rgb(76,  86,  106),
    overlay_bg: Color::Rgb(46,  52,  64),
    overlay_fg: Color::Rgb(216, 222, 233),
    overlay_key: Color::Rgb(136, 192, 208),
};

pub const GRUVBOX: Theme = Theme {
    ok:         Color::Rgb(184, 187, 38),
    warn:       Color::Rgb(215, 153, 33),
    error:      Color::Rgb(204, 36,  29),
    muted:      Color::Rgb(146, 131, 116),
    overlay_bg: Color::Rgb(40,  40,  40),
    overlay_fg: Color::Rgb(235, 219, 178),
    overlay_key: Color::Rgb(250, 189, 47),
};

/// Theme returned when `NO_COLOR` is set: every role resolves to
/// `Color::Reset`, so render code that consumes a `Theme` ends up
/// emitting the terminal's default foreground / background — no
/// special branching required at every call site.
pub const NO_COLOR_THEME: Theme = Theme {
    ok:          Color::Reset,
    warn:        Color::Reset,
    error:       Color::Reset,
    muted:       Color::Reset,
    overlay_bg:  Color::Reset,
    overlay_fg:  Color::Reset,
    overlay_key: Color::Reset,
};

impl Theme {
    /// Block factory for an *outermost* view container — Scorecard frame,
    /// Spotlight frame, logs overlay frame, popup frame. Carries
    /// `Borders::ALL`. Pairs with [`Theme::plain_block`] so call sites
    /// express intent (frame vs. inner widget) rather than copying border
    /// flags around. See ADR-0008 §"Container vs. plain block split".
    pub fn container_block(&self) -> Block<'static> {
        Block::default().borders(Borders::ALL)
    }

    /// Block factory for an *inner* widget — anything inside an outer
    /// frame that should not draw its own border. The Block still
    /// accepts a title (rendered on its top row) so headers and section
    /// labels keep their text. See ADR-0008 §"Container vs. plain block
    /// split".
    pub fn plain_block(&self) -> Block<'static> {
        Block::default()
    }

    pub fn from_name(s: &str) -> Result<Theme, String> {
        match s {
            "default" => Ok(DEFAULT),
            "dracula"  => Ok(DRACULA),
            "nord"     => Ok(NORD),
            "gruvbox"  => Ok(GRUVBOX),
            _ => Err(format!(
                "unknown theme {:?}; valid names: default, dracula, nord, gruvbox",
                s
            )),
        }
    }
}

pub fn resolve_theme(flag: Option<&str>) -> Result<Theme, String> {
    resolve_theme_with(
        flag,
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var("NICO_THEME").ok().as_deref(),
    )
}

/// Pure resolver — env vars are passed in, no global state read. Tests
/// target this directly so they can run in parallel without env-var
/// races. The thin `resolve_theme` wrapper just plumbs in real env.
pub fn resolve_theme_with(
    flag: Option<&str>,
    no_color: bool,
    nico_theme_env: Option<&str>,
) -> Result<Theme, String> {
    // Per https://no-color.org and ADR-0008 precedence rule #4:
    // presence of NO_COLOR (any value, including empty) disables color
    // entirely, overriding both the --theme flag and NICO_THEME.
    if no_color {
        return Ok(NO_COLOR_THEME);
    }
    if let Some(name) = flag {
        return Theme::from_name(name);
    }
    if let Some(name) = nico_theme_env {
        return Theme::from_name(name);
    }
    Ok(DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- color-science helpers (used by visual-QA tests) ---

    fn rgb_vals(c: Color) -> (u8, u8, u8) {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => panic!("expected Color::Rgb, got {c:?}"),
        }
    }

    fn rgb_distance(a: Color, b: Color) -> f64 {
        let (r1, g1, b1) = rgb_vals(a);
        let (r2, g2, b2) = rgb_vals(b);
        let (dr, dg, db) = (r1 as f64 - r2 as f64, g1 as f64 - g2 as f64, b1 as f64 - b2 as f64);
        (dr*dr + dg*dg + db*db).sqrt()
    }

    fn linearize(c: u8) -> f64 {
        let v = c as f64 / 255.0;
        if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    }

    fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
        0.2126 * linearize(r) + 0.7152 * linearize(g) + 0.0722 * linearize(b)
    }

    fn contrast_ratio(fg: Color, bg: Color) -> f64 {
        let (r1, g1, b1) = rgb_vals(fg);
        let (r2, g2, b2) = rgb_vals(bg);
        let l1 = relative_luminance(r1, g1, b1);
        let l2 = relative_luminance(r2, g2, b2);
        let (lighter, darker) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (lighter + 0.05) / (darker + 0.05)
    }

    // --- visual QA tests (acceptance criteria for issue #99) ---

    // AC: status indicators (ok, warn, error, muted) are visually distinct
    #[test]
    fn default_status_indicators_are_pairwise_distinct() {
        let indicators = [
            ("ok",    DEFAULT.ok),
            ("warn",  DEFAULT.warn),
            ("error", DEFAULT.error),
            ("muted", DEFAULT.muted),
        ];
        let min_dist = 80.0_f64;
        for i in 0..indicators.len() {
            for j in (i+1)..indicators.len() {
                let (na, ca) = indicators[i];
                let (nb, cb) = indicators[j];
                let d = rgb_distance(ca, cb);
                assert!(
                    d >= min_dist,
                    "default: {na} vs {nb} distance={d:.1} < {min_dist}; colors too similar to distinguish"
                );
            }
        }
    }

    // AC: overlay background/foreground are legible (WCAG AA >= 4.5)
    #[test]
    fn all_themes_overlay_fg_contrast_meets_wcag_aa() {
        let min = 4.5_f64;
        for (name, theme) in [("default", DEFAULT), ("dracula", DRACULA), ("nord", NORD), ("gruvbox", GRUVBOX)] {
            let ratio = contrast_ratio(theme.overlay_fg, theme.overlay_bg);
            assert!(
                ratio >= min,
                "theme={name}: overlay_fg/overlay_bg contrast {ratio:.2} < {min} (WCAG AA)"
            );
        }
    }

    // AC: overlay key color is legible over the overlay background
    #[test]
    fn all_themes_overlay_key_contrast_meets_wcag_aa() {
        let min = 4.5_f64;
        for (name, theme) in [("default", DEFAULT), ("dracula", DRACULA), ("nord", NORD), ("gruvbox", GRUVBOX)] {
            let ratio = contrast_ratio(theme.overlay_key, theme.overlay_bg);
            assert!(
                ratio >= min,
                "theme={name}: overlay_key/overlay_bg contrast {ratio:.2} < {min} (WCAG AA)"
            );
        }
    }

    // AC: grey/white palette is replaced — ok/warn/error must be chromatic (channel spread >= 50)
    #[test]
    fn default_ok_warn_error_are_chromatic_not_grey() {
        let min_spread = 50u8;
        for (name, color) in [("ok", DEFAULT.ok), ("warn", DEFAULT.warn), ("error", DEFAULT.error)] {
            let (r, g, b) = rgb_vals(color);
            let spread = r.max(g).max(b) - r.min(g).min(b);
            assert!(
                spread >= min_spread,
                "default: {name} ({r},{g},{b}) channel spread={spread} < {min_spread}; looks grey/white"
            );
        }
    }

    #[test]
    fn default_theme_has_correct_ok_color() {
        assert_eq!(DEFAULT.ok, Color::Rgb(97, 175, 74));
    }

    #[test]
    fn dracula_theme_has_correct_ok_color() {
        assert_eq!(DRACULA.ok, Color::Rgb(80, 250, 123));
    }

    #[test]
    fn nord_theme_has_correct_ok_color() {
        assert_eq!(NORD.ok, Color::Rgb(163, 190, 140));
    }

    #[test]
    fn gruvbox_theme_has_correct_ok_color() {
        assert_eq!(GRUVBOX.ok, Color::Rgb(184, 187, 38));
    }

    #[test]
    fn from_name_default_returns_default_theme() {
        assert_eq!(Theme::from_name("default").unwrap(), DEFAULT);
    }

    #[test]
    fn from_name_dracula_returns_dracula_theme() {
        assert_eq!(Theme::from_name("dracula").unwrap(), DRACULA);
    }

    #[test]
    fn from_name_nord_returns_nord_theme() {
        assert_eq!(Theme::from_name("nord").unwrap(), NORD);
    }

    #[test]
    fn from_name_gruvbox_returns_gruvbox_theme() {
        assert_eq!(Theme::from_name("gruvbox").unwrap(), GRUVBOX);
    }

    #[test]
    fn from_name_invalid_returns_err_listing_valid_names() {
        let err = Theme::from_name("solarized").unwrap_err();
        assert!(err.contains("solarized"), "error should echo the bad name: {err}");
        assert!(err.contains("default"),   "error should list valid names: {err}");
        assert!(err.contains("dracula"),   "error should list valid names: {err}");
        assert!(err.contains("nord"),      "error should list valid names: {err}");
        assert!(err.contains("gruvbox"),   "error should list valid names: {err}");
    }

    #[test]
    fn resolve_theme_flag_wins_over_env() {
        let result = resolve_theme_with(Some("dracula"), false, Some("nord")).unwrap();
        assert_eq!(result, DRACULA);
    }

    #[test]
    fn resolve_theme_env_used_when_no_flag() {
        let result = resolve_theme_with(None, false, Some("gruvbox")).unwrap();
        assert_eq!(result, GRUVBOX);
    }

    #[test]
    fn resolve_theme_defaults_when_no_flag_no_env() {
        let result = resolve_theme_with(None, false, None).unwrap();
        assert_eq!(result, DEFAULT);
    }

    #[test]
    fn resolve_theme_invalid_flag_returns_err() {
        let err = resolve_theme_with(Some("bad-theme"), false, None).unwrap_err();
        assert!(err.contains("bad-theme"));
    }

    #[test]
    fn resolve_theme_with_no_color_returns_no_color_theme() {
        let result = resolve_theme_with(None, true, None).unwrap();
        assert_eq!(result, NO_COLOR_THEME);
    }

    #[test]
    fn resolve_theme_with_no_color_overrides_flag() {
        let result = resolve_theme_with(Some("dracula"), true, None).unwrap();
        assert_eq!(result, NO_COLOR_THEME, "NO_COLOR must override --theme flag");
    }

    #[test]
    fn resolve_theme_with_no_color_overrides_nico_theme_env() {
        let result = resolve_theme_with(None, true, Some("gruvbox")).unwrap();
        assert_eq!(result, NO_COLOR_THEME, "NO_COLOR must override NICO_THEME");
    }

    #[test]
    fn no_color_theme_is_all_reset() {
        assert_eq!(NO_COLOR_THEME.ok, Color::Reset);
        assert_eq!(NO_COLOR_THEME.warn, Color::Reset);
        assert_eq!(NO_COLOR_THEME.error, Color::Reset);
        assert_eq!(NO_COLOR_THEME.muted, Color::Reset);
        assert_eq!(NO_COLOR_THEME.overlay_bg, Color::Reset);
        assert_eq!(NO_COLOR_THEME.overlay_fg, Color::Reset);
        assert_eq!(NO_COLOR_THEME.overlay_key, Color::Reset);
    }

    #[test]
    fn resolve_theme_with_invalid_flag_returns_err_even_without_no_color() {
        let err = resolve_theme_with(Some("solarized"), false, None).unwrap_err();
        assert!(err.contains("solarized"));
    }

    // --- container_block / plain_block split (issue #370) ---

    use ratatui::layout::Rect;

    #[test]
    fn container_block_shrinks_inner_area_by_borders() {
        // A bordered block (Borders::ALL) shrinks `inner(area)` by 1 on
        // each side. That's how render code expresses "this widget is an
        // outermost frame with a visible border".
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let block = DEFAULT.container_block();
        let inner = block.inner(area);
        assert_eq!(
            inner.width, 8,
            "container_block inner should be area - 2 columns"
        );
        assert_eq!(
            inner.height, 8,
            "container_block inner should be area - 2 rows"
        );
    }

    #[test]
    fn plain_block_leaves_inner_area_unchanged() {
        // No borders → `inner(area)` returns the area untouched. Render
        // code uses this to express "this is an inner widget; don't draw
        // a frame around it".
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let block = DEFAULT.plain_block();
        let inner = block.inner(area);
        assert_eq!(inner.width, 10);
        assert_eq!(inner.height, 10);
    }

    #[test]
    fn all_theme_fields_are_rgb_variants() {
        for (name, theme) in [
            ("default", DEFAULT),
            ("dracula", DRACULA),
            ("nord", NORD),
            ("gruvbox", GRUVBOX),
        ] {
            for (field, color) in [
                ("ok", theme.ok),
                ("warn", theme.warn),
                ("error", theme.error),
                ("muted", theme.muted),
                ("overlay_bg", theme.overlay_bg),
                ("overlay_fg", theme.overlay_fg),
                ("overlay_key", theme.overlay_key),
            ] {
                assert!(
                    matches!(color, Color::Rgb(..)),
                    "theme={name} field={field} should be Color::Rgb, got {color:?}"
                );
            }
        }
    }
}
