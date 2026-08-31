//! Запасной шрифт для символов, которых нет во встроенном наборе egui.
//!
//! Встроенный Ubuntu-Light покрывает кириллицу и латиницу, но стрелки, знак
//! предупреждения и клавиатурные символы в нём отсутствуют — вместо них
//! рисуются пустые квадраты. Apple Symbols есть в любой macOS и весит меньше
//! мегабайта, поэтому подключаем его как запасной, а не тащим шрифт в бандл.

use std::sync::Arc;

const CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Apple Symbols.ttf",
    "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
];

pub fn install(ctx: &egui::Context) {
    let Some((path, bytes)) = CANDIDATES
        .iter()
        .find_map(|p| std::fs::read(p).ok().map(|b| (*p, b)))
    else {
        log::warn!("запасной шрифт не найден — часть значков может не отрисоваться");
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "symbols".to_owned(),
        Arc::new(egui::FontData::from_owned(bytes)),
    );
    // В конец списка: основной шрифт остаётся основным, запасной подхватывает
    // только те символы, которых в нём нет.
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("symbols".to_owned());
    }
    ctx.set_fonts(fonts);
    log::info!("запасной шрифт: {path}");
}
