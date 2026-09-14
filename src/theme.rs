//! Единая визуальная тема окна настроек.
//!
//! Системная тёмная тема egui рассчитана на технические демо: вторичный текст
//! получается слишком тусклым, а кнопки и поля почти сливаются с фоном. Здесь
//! задаём спокойную палитру и одинаковые радиусы, чтобы интерфейс читался как
//! цельное macOS-приложение.

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle};

pub fn install(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 7.0);
        style.spacing.button_padding = egui::vec2(11.0, 6.0);
        style.spacing.interact_size = egui::vec2(40.0, 30.0);
        style.spacing.window_margin = Margin::same(14);
        style.spacing.menu_margin = Margin::same(8);
        style.spacing.indent = 18.0;

        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(24.0, FontFamily::Proportional),
        );
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(13.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        );

        let visuals = &mut style.visuals;
        visuals.dark_mode = true;
        visuals.override_text_color = Some(Color32::from_rgb(235, 240, 245));
        visuals.weak_text_color = Some(Color32::from_rgb(157, 169, 182));
        visuals.panel_fill = Color32::from_rgb(23, 27, 33);
        visuals.window_fill = Color32::from_rgb(18, 22, 28);
        visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(52, 61, 72));
        visuals.window_corner_radius = CornerRadius::same(12);
        visuals.menu_corner_radius = CornerRadius::same(10);
        visuals.extreme_bg_color = Color32::from_rgb(13, 17, 22);
        visuals.text_edit_bg_color = Some(Color32::from_rgb(15, 20, 26));
        visuals.faint_bg_color = Color32::from_rgb(31, 37, 45);
        visuals.selection.bg_fill = Color32::from_rgb(24, 117, 159);
        visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(117, 215, 255));
        visuals.hyperlink_color = Color32::from_rgb(100, 202, 255);
        visuals.warn_fg_color = Color32::from_rgb(246, 177, 75);
        visuals.error_fg_color = Color32::from_rgb(255, 111, 126);

        let border = Stroke::new(1.0, Color32::from_rgb(54, 65, 77));
        let normal_text = Stroke::new(1.0, Color32::from_rgb(220, 228, 236));
        let muted_text = Stroke::new(1.0, Color32::from_rgb(160, 173, 187));
        let hover_text = Stroke::new(1.5, Color32::from_rgb(245, 250, 255));
        let states = [
            (
                &mut visuals.widgets.noninteractive,
                Color32::TRANSPARENT,
                muted_text,
            ),
            (
                &mut visuals.widgets.inactive,
                Color32::from_rgb(34, 42, 51),
                normal_text,
            ),
            (
                &mut visuals.widgets.hovered,
                Color32::from_rgb(44, 57, 70),
                hover_text,
            ),
            (
                &mut visuals.widgets.active,
                Color32::from_rgb(25, 129, 171),
                hover_text,
            ),
            (
                &mut visuals.widgets.open,
                Color32::from_rgb(39, 51, 63),
                normal_text,
            ),
        ];
        for (widget, fill, text) in states {
            widget.weak_bg_fill = fill;
            widget.bg_fill = fill;
            widget.bg_stroke = border;
            widget.fg_stroke = text;
            widget.corner_radius = CornerRadius::same(8);
        }
    });
}
