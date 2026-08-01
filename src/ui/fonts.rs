pub fn scan_system_fonts() -> Vec<(String, String)> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut seen = std::collections::HashSet::new();
    let mut fonts: Vec<(String, String)> = Vec::new();

    for face in db.faces() {
        if let Some((family, _)) = face.families.first() {
            if seen.insert(family.clone()) {
                if let fontdb::Source::File(path) = &face.source {
                    fonts.push((family.clone(), path.to_string_lossy().into_owned()));
                }
            }
        }
    }

    fonts.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    fonts
}

pub fn family_from_file(path: &str) -> Option<String> {
    let mut db = fontdb::Database::new();
    db.load_font_file(path).ok()?;
    let name = db.faces().next()?.families.first()?.0.clone();
    Some(name)
}

fn unicode_fallback_path() -> Option<std::path::PathBuf> {
    const PREFERRED_FAMILIES: [&str; 2] = ["Noto Sans", "DejaVu Sans"];
    const COMMON_PATHS: [&str; 2] = [
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];

    if let Some(path) = COMMON_PATHS
        .iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
    {
        return Some(path);
    }

    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    PREFERRED_FAMILIES.iter().find_map(|wanted| {
        db.faces().find_map(|face| {
            let has_family = face.families.iter().any(|(family, _)| family == wanted);
            match (&face.source, has_family) {
                (fontdb::Source::File(path), true) => Some(path.clone()),
                _ => None,
            }
        })
    })
}

pub fn apply(ctx: &egui::Context, font_path: &str) {
    let mut fonts = egui::FontDefinitions::default();
    if !font_path.is_empty() {
        if let Ok(data) = std::fs::read(font_path) {
            fonts.font_data.insert(
                "user_font".to_owned(),
                egui::FontData::from_owned(data),
            );
            fonts.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "user_font".to_owned());
            fonts.families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "user_font".to_owned());
        }
    }
    // egui's bundled fonts intentionally cover only a small character set.
    // Keep the selected face first, but add a broad system fallback so Matrix
    // display names containing IPA, combining marks, or non-Latin scripts do
    // not turn into tofu boxes. Emoji remain handled separately by Twemoji.
    let mut profile_fonts = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(path) = unicode_fallback_path() {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert(
                "unicode_fallback".to_owned(),
                egui::FontData::from_owned(data),
            );
            for family in [
                egui::FontFamily::Proportional,
                egui::FontFamily::Monospace,
            ] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("unicode_fallback".to_owned());
            }
            profile_fonts.insert(0, "unicode_fallback".to_owned());
        }
    }
    // Always bind the named family. On a minimal system without Noto/DejaVu it
    // gracefully uses egui's proportional fonts instead of panicking.
    fonts.families.insert(
        egui::FontFamily::Name("unicode_fallback".into()),
        profile_fonts,
    );
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_fallback_covers_ipa_display_names() {
        let ctx = egui::Context::default();
        super::apply(&ctx, "");
        ctx.begin_frame(egui::RawInput::default());
        assert!(ctx.fonts(|fonts| {
            fonts.has_glyphs(&egui::FontId::proportional(16.0), "ˈt͡sɛːzaɐ̯")
        }));
        assert!(ctx.fonts(|fonts| {
            fonts.has_glyphs(
                &egui::FontId::new(
                    16.0,
                    egui::FontFamily::Name("unicode_fallback".into()),
                ),
                "ˈt͡sɛːzaɐ̯",
            )
        }));
        let _ = ctx.end_frame();
    }
}
