//! Обычная пунктуация в диктовке и результатах текстовых действий.

const INSTRUCTION: &str = "Оформление результата: используй обычную пунктуацию. \
Вместо длинных и средних тире используй символ -; вместо типографских двойных \
кавычек, в том числе елочек, используй прямые кавычки \"; вместо типографских \
одинарных кавычек и апострофов используй символ '. Вместо знака многоточия \
пиши три точки ...; вместо неразрывных пробелов используй обычные пробелы. \
Сохраняй язык текста, буквы с диакритикой, переносы строк и отступы. \
Не добавляй кавычки вокруг всего ответа или пояснения об оформлении.";

/// Дополняет и уже сохранённые пользовательские промпты, без миграции настроек.
pub fn prompt(system: &str) -> String {
    format!("{system}\n\n{INSTRUCTION}")
}

/// Промпт не гарантирует соблюдение оформления, поэтому нормализуем и ответ.
/// Меняем только перечисленные варианты пунктуации, а не весь не-ASCII текст.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{2010}'..='\u{2015}' | '\u{2e3a}' | '\u{2e3b}' => out.push('-'),
            '«' | '»' | '“' | '”' | '„' | '‟' => out.push('"'),
            '‹' | '›' | '‘' | '’' | '‚' | '‛' => out.push('\''),
            '…' => out.push_str("..."),
            '\u{00a0}' | '\u{2007}' | '\u{202f}' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_russian_and_czech_typography() {
        assert_eq!(
            normalize("Он сказал: «Да» — и замолчал…\n„Dobrý den,“ řekl – v 10\u{00a0}h."),
            "Он сказал: \"Да\" - и замолчал...\n\"Dobrý den,\" řekl - v 10 h."
        );
    }

    #[test]
    fn normalizes_apostrophes_and_nonbreaking_characters() {
        assert_eq!(
            normalize("‘Don’t’ ‹stop› 1\u{202f}000 x\u{2007}y a\u{2011}b"),
            "'Don't' 'stop' 1 000 x y a-b"
        );
    }

    #[test]
    fn preserves_languages_layout_and_unrelated_symbols() {
        let text = "  Příliš žluťoučký kůň\n\tПривет, їжак, café, 中文 🙂\n- x = −2; 5 €; 2 × 3\n{\"ok\": true}";
        assert_eq!(normalize(text), text);
    }

    #[test]
    fn normalization_is_idempotent() {
        let once = normalize("«Текст» — “text”… ‘slovo’\u{00a0}42");
        assert_eq!(normalize(&once), once);
    }

    #[test]
    fn adds_instructions_without_rewriting_custom_prompt() {
        let custom = "Переведи на чешский. Сохрани список.";
        assert_eq!(prompt(custom), format!("{custom}\n\n{INSTRUCTION}"));
    }
}
