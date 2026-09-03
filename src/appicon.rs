//! Значки источников для истории буфера.
//!
//! Откуда скопировано, приложение уже знает: `NSWorkspace` называет
//! программу, которая была впереди в момент копирования. Здесь из этого
//! делается картинка — настоящий значок программы, взятый у системы.
//!
//! Значки грузятся по требованию и остаются в кэше: рисование NSImage в
//! растр стоит миллисекунд, а список перерисовывается десятки раз в секунду.
//! Неудача тоже запоминается — программу могли удалить, и долбиться в неё
//! каждый кадр незачем.

use crate::clipboard::Entry;
use eframe::egui;
use std::collections::HashMap;

/// Сторона растра, в который рисуется значок. Показываем его вдвое меньше:
/// на экране с удвоенной плотностью точек значок 16 на 16 выглядел бы мылом.
const TEXTURE_SIDE: usize = 64;

/// Вид программы. Нужен для цвета и для запасного значка, когда настоящий
/// взять неоткуда: у записей, сделанных до появления этой возможности,
/// сохранено только имя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Browser,
    Terminal,
    Code,
    Notes,
    Mail,
    Chat,
    Docs,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Browser => "браузер",
            Kind::Terminal => "терминал",
            Kind::Code => "редактор кода",
            Kind::Notes => "заметки",
            Kind::Mail => "почта",
            Kind::Chat => "переписка",
            Kind::Docs => "документы",
            Kind::Other => "программа",
        }
    }

    /// Цвета разведены по тону, а не по яркости: список читается и на тёмной
    /// плашке выбора, и на светлом окне настроек.
    pub fn color(self) -> egui::Color32 {
        match self {
            Kind::Browser => egui::Color32::from_rgb(70, 130, 220),
            Kind::Terminal => egui::Color32::from_rgb(90, 175, 120),
            Kind::Code => egui::Color32::from_rgb(150, 120, 210),
            Kind::Notes => egui::Color32::from_rgb(215, 165, 60),
            Kind::Mail => egui::Color32::from_rgb(215, 110, 90),
            Kind::Chat => egui::Color32::from_rgb(80, 170, 195),
            Kind::Docs => egui::Color32::from_rgb(130, 140, 155),
            Kind::Other => egui::Color32::from_rgb(120, 120, 132),
        }
    }
}

/// Разбирает вид программы по идентификатору бандла, а если его нет — по
/// имени. Идентификатор надёжнее: имя переводится на язык системы и меняется
/// от версии к версии, а `com.apple.Safari` остаётся собой.
pub fn kind_of(entry: &Entry) -> Kind {
    if let Some(id) = entry.app_id.as_deref() {
        let id = id.to_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|n| id.contains(n));
        if has(&[
            "safari",
            "chrome",
            "firefox",
            "edgemac",
            "thebrowser",
            "brave",
            "vivaldi",
            "opera",
            "yandex-browser",
            "chromium",
            "orion",
            "zen-browser",
        ]) {
            return Kind::Browser;
        }
        if has(&[
            "terminal",
            "iterm",
            "warp",
            "kitty",
            "ghostty",
            "alacritty",
            "wezterm",
            "tabby",
        ]) {
            return Kind::Terminal;
        }
        if has(&[
            "vscode",
            "xcode",
            "jetbrains",
            "sublime",
            "zed",
            "cursor",
            "nova",
            "fleet",
            "android studio",
            "godot",
        ]) {
            return Kind::Code;
        }
        if has(&[
            "notes",
            "obsidian",
            "notion",
            "evernote",
            "bear",
            "drafts",
            "craft",
            "logseq",
            "things",
            "reminders",
            "anytype",
        ]) {
            return Kind::Notes;
        }
        if has(&[
            "mail",
            "outlook",
            "spark",
            "thunderbird",
            "airmail",
            "canary",
        ]) {
            return Kind::Mail;
        }
        if has(&[
            "telegram", "slack", "discord", "whatsapp", "zoom", "messages", "signal", "skype",
            "teams", "viber",
        ]) {
            return Kind::Chat;
        }
        if has(&[
            "pages",
            "numbers",
            "keynote",
            "word",
            "excel",
            "powerpoint",
            "preview",
            "acrobat",
            "finder",
            "libreoffice",
            "pdfexpert",
        ]) {
            return Kind::Docs;
        }
        return Kind::Other;
    }

    // Старые записи: только имя. Разбор по нему грубее, но лучше, чем ничего.
    let Some(name) = entry.source.as_deref() else {
        return Kind::Other;
    };
    let name = name.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| name.contains(n));
    if has(&[
        "safari",
        "chrome",
        "firefox",
        "edge",
        "brave",
        "браузер",
        "vivaldi",
    ]) {
        Kind::Browser
    } else if has(&["terminal", "терминал", "iterm", "warp", "kitty", "ghostty"]) {
        Kind::Terminal
    } else if has(&["code", "xcode", "sublime", "zed", "cursor", "studio"]) {
        Kind::Code
    } else if has(&["notes", "заметки", "obsidian", "notion", "bear"]) {
        Kind::Notes
    } else if has(&["mail", "почта", "outlook", "spark"]) {
        Kind::Mail
    } else if has(&[
        "telegram",
        "slack",
        "discord",
        "whatsapp",
        "сообщения",
        "messages",
    ]) {
        Kind::Chat
    } else if has(&["pages", "word", "excel", "preview", "просмотр", "finder"]) {
        Kind::Docs
    } else {
        Kind::Other
    }
}

/// Настоящие значки программ, уже загруженные в видеопамять.
///
/// Ключ — путь к бандлу. `None` означает «пробовали, не вышло»: без этого
/// отсутствующая программа перезапускала бы поиск на каждом кадре.
#[derive(Default)]
pub struct Icons {
    loaded: HashMap<String, Option<egui::TextureHandle>>,
    /// Пути, найденные по идентификатору бандла, — для записей, где пути нет.
    paths: HashMap<String, Option<String>>,
}

impl Icons {
    /// Значок программы-источника, если система смогла его отдать.
    fn texture(&mut self, ctx: &egui::Context, entry: &Entry) -> Option<&egui::TextureHandle> {
        let path = self.path_for(entry)?;
        if !self.loaded.contains_key(&path) {
            let texture = crate::macos::app_icon_rgba(&path, TEXTURE_SIDE).map(|rgba| {
                // Растр из AppKit уже с домноженной альфой — как и Color32.
                let image =
                    egui::ColorImage::from_rgba_premultiplied([TEXTURE_SIDE, TEXTURE_SIDE], &rgba);
                ctx.load_texture(
                    format!("appicon:{path}"),
                    image,
                    egui::TextureOptions::LINEAR,
                )
            });
            if texture.is_none() {
                log::debug!("значок для {path} не получен");
            }
            self.loaded.insert(path.clone(), texture);
        }
        self.loaded.get(&path)?.as_ref()
    }

    /// Путь к бандлу: сохранённый в записи или найденный по идентификатору.
    fn path_for(&mut self, entry: &Entry) -> Option<String> {
        if let Some(path) = entry.app_path.as_deref() {
            return Some(path.to_string());
        }
        let id = entry.app_id.as_deref()?;
        if !self.paths.contains_key(id) {
            let found = crate::macos::app_path_for_id(id);
            self.paths.insert(id.to_string(), found);
        }
        self.paths.get(id)?.clone()
    }

    /// Рисует значок источника в заданном месте.
    ///
    /// Если настоящего значка нет — цветной квадратик с первой буквой имени.
    /// Пустое место было бы хуже: строки списка разъехались бы по левому краю,
    /// и глазу не за что было бы зацепиться.
    pub fn draw(&mut self, ui: &egui::Ui, rect: egui::Rect, entry: &Entry) {
        let ctx = ui.ctx().clone();
        if let Some(texture) = self.texture(&ctx, entry) {
            ui.painter().image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            return;
        }

        let kind = kind_of(entry);
        let painter = ui.painter();
        painter.rect_filled(rect, egui::CornerRadius::same(4), kind.color());
        let letter = entry
            .source
            .as_deref()
            .and_then(|s| s.chars().find(|c| c.is_alphanumeric()))
            .unwrap_or('?')
            .to_uppercase()
            .to_string();
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            letter,
            egui::FontId::proportional(rect.height() * 0.62),
            egui::Color32::WHITE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{kind_of, Kind};
    use crate::clipboard::Entry;

    fn entry(id: Option<&str>, name: Option<&str>) -> Entry {
        Entry {
            at: chrono::Local::now(),
            text: "текст".into(),
            source: name.map(str::to_string),
            app_id: id.map(str::to_string),
            app_path: None,
        }
    }

    #[test]
    fn вид_разбирается_по_идентификатору() {
        assert_eq!(
            kind_of(&entry(Some("com.apple.Safari"), None)),
            Kind::Browser
        );
        assert_eq!(
            kind_of(&entry(Some("com.apple.Terminal"), None)),
            Kind::Terminal
        );
        assert_eq!(kind_of(&entry(Some("com.apple.Notes"), None)), Kind::Notes);
        assert_eq!(kind_of(&entry(Some("com.apple.mail"), None)), Kind::Mail);
        assert_eq!(
            kind_of(&entry(Some("com.microsoft.VSCode"), None)),
            Kind::Code
        );
        assert_eq!(
            kind_of(&entry(Some("com.tinyspeck.slackmacgap"), None)),
            Kind::Chat
        );
    }

    #[test]
    fn идентификатор_важнее_имени() {
        // Имя переводится на язык системы, идентификатор — нет.
        let e = entry(Some("com.apple.Notes"), Some("Заметки"));
        assert_eq!(kind_of(&e), Kind::Notes);
    }

    #[test]
    fn терминал_на_основе_хрома_не_считается_браузером() {
        // Порядок проверок важен: у части терминалов в идентификаторе есть
        // слова, попадающие и в другие списки.
        assert_eq!(
            kind_of(&entry(Some("com.googlecode.iterm2"), None)),
            Kind::Terminal
        );
    }

    #[test]
    fn старые_записи_разбираются_по_имени() {
        // До версии 0.15.0 идентификатор не сохранялся — остаётся имя.
        assert_eq!(kind_of(&entry(None, Some("Safari"))), Kind::Browser);
        assert_eq!(kind_of(&entry(None, Some("Терминал"))), Kind::Terminal);
        assert_eq!(kind_of(&entry(None, Some("Заметки"))), Kind::Notes);
    }

    #[test]
    fn незнакомая_программа_не_выдаёт_себя_за_браузер() {
        // Короткие куски вроде «arc» ловили бы «Search» и «Charles».
        assert_eq!(kind_of(&entry(None, Some("Charles"))), Kind::Other);
        assert_eq!(kind_of(&entry(None, Some("Search"))), Kind::Other);
        assert_eq!(kind_of(&entry(None, None)), Kind::Other);
    }
}
