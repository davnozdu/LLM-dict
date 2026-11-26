//! Защита от порчи текста моделью правки.
//!
//! Модель правки (seq2seq) переписывает фразу целиком и иногда выбрасывает
//! слова, подменяет редкие частыми или зацикливается. Замер на 200 реальных
//! диктовках показал, что без защиты порчи больше, чем пользы. Поэтому из её
//! ответа берутся только безопасные правки, по словам:
//!
//! - знаки препинания и регистр — да;
//! - опечатка или окончание: та же первая буква, не больше двух правок — да;
//! - «е» → «ё» — да;
//! - кириллица → название латиницей с похожей транслитерацией («гитхаб» → «GitHub») — да;
//! - выбросить слово — только повтор соседнего (заикание);
//! - новое слово или кавычки, которых не было, — нет;
//! - конечный знак остаётся исходным; к оборванной фразе можно добавить только «?»;
//! - зацикливание или раздувание текста — откат к исходному.
//!
//! Правила те же, что в `autotrain/guard.py` репозитория speechcorrector, где на них
//! считаются метрики.

/// Сердцевина слова для сравнения: строчные буквы и цифры без знаков.
fn core(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn lev(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + usize::from(ca != cb)));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn lev_str(a: &str, b: &str) -> usize {
    lev(&a.chars().collect::<Vec<_>>(), &b.chars().collect::<Vec<_>>())
}

/// Одно и то же слово с исправленной опечаткой или окончанием.
fn similar(a: &str, b: &str) -> bool {
    let (a, b) = (core(a), core(b));
    if a.replace('ё', "е") == b.replace('ё', "е") {
        return true;
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    ca.len() >= 4
        && cb.len() >= 4
        && ca[0] == cb[0]
        && lev(&ca, &cb) <= 2
        && ca.len().abs_diff(cb.len()) <= 2
}

fn translit(c: char) -> Option<&'static str> {
    const TABLE: [(char, &str); 33] = [
        ('а', "a"), ('б', "b"), ('в', "v"), ('г', "g"), ('д', "d"), ('е', "e"), ('ё', "e"),
        ('ж', "zh"), ('з', "z"), ('и', "i"), ('й', "y"), ('к', "k"), ('л', "l"), ('м', "m"),
        ('н', "n"), ('о', "o"), ('п', "p"), ('р', "r"), ('с', "s"), ('т', "t"), ('у', "u"),
        ('ф', "f"), ('х', "h"), ('ц', "ts"), ('ч', "ch"), ('ш', "sh"), ('щ', "sch"), ('ъ', ""),
        ('ы', "y"), ('ь', ""), ('э', "e"), ('ю', "yu"), ('я', "ya"),
    ];
    TABLE.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

fn norm_lat(s: &str) -> String {
    let mut s = s.to_owned();
    for (a, b) in [
        ("ee", "i"), ("ea", "i"), ("oo", "u"), ("ph", "f"), ("x", "ks"),
        ("c", "k"), ("q", "k"), ("w", "v"), ("y", "i"), ("j", "dzh"),
    ] {
        s = s.replace(a, b);
    }
    s
}

const NUMBER_WORDS: [&str; 14] = [
    "ноль", "один", "одна", "два", "две", "три", "четыре", "пять", "шесть",
    "семь", "восемь", "девять", "десять", "точка",
];

/// Кириллическая запись названия заменена им же латиницей.
fn translit_ok(raw: &[&str], out: &[&str]) -> bool {
    let cx: Vec<String> = raw.iter().map(|w| core(w)).collect();
    let cy: Vec<String> = out.iter().map(|w| core(w)).collect();
    let cyr = |w: &String| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit() || translit(c).is_some());
    let lat = |w: &String| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !cx.iter().all(cyr) || !cy.iter().all(lat) {
        return false;
    }
    let a: String = cx.iter().filter(|w| !NUMBER_WORDS.contains(&w.as_str())).cloned().collect();
    let b: String = cy.concat().chars().filter(|c| !c.is_ascii_digit()).collect();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let t = norm_lat(&a.chars().map(|c| translit(c).unwrap_or("")).collect::<String>());
    let b = norm_lat(&b);
    lev_str(&t, &b) <= 2.max(b.chars().count().div_ceil(2))
}

/// Подстрока длиной 2–12 символов, повторённая подряд четыре раза и больше.
fn has_loop(s: &str) -> bool {
    let c: Vec<char> = s.chars().collect();
    for p in 2..=12 {
        for start in 0..c.len() {
            let mut reps = 1;
            while start + (reps + 1) * p <= c.len()
                && c[start + reps * p..start + (reps + 1) * p] == c[start..start + p]
            {
                reps += 1;
            }
            if reps >= 4 {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, PartialEq)]
enum Op {
    Equal(usize, usize, usize, usize),
    Replace(usize, usize, usize, usize),
    Delete(usize, usize),
    Insert(usize, usize),
}

/// Выравнивание двух последовательностей слов по наибольшей общей подпоследовательности.
fn opcodes(a: &[String], b: &[String]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs.push((n, m));
    let mut ops = Vec::new();
    let (mut pi, mut pj) = (0, 0);
    for (i, j) in pairs {
        match (i > pi, j > pj) {
            (true, true) => ops.push(Op::Replace(pi, i, pj, j)),
            (true, false) => ops.push(Op::Delete(pi, i)),
            (false, true) => ops.push(Op::Insert(pj, j)),
            (false, false) => {}
        }
        if i < n {
            match ops.last_mut() {
                Some(Op::Equal(_, a2, _, b2)) if *a2 == i && *b2 == j => {
                    *a2 += 1;
                    *b2 += 1;
                }
                _ => ops.push(Op::Equal(i, i + 1, j, j + 1)),
            }
        }
        pi = i + 1;
        pj = j + 1;
    }
    ops
}

fn split_end(s: &str) -> (&str, &str) {
    let body = s.trim_end_matches(['.', '?', '!', '…']);
    (body, &s[body.len()..])
}

/// Принимает из ответа модели `out` только безопасные правки исходного текста `raw`.
pub fn merge(raw: &str, out: &str) -> String {
    if raw.trim().is_empty() {
        return raw.to_owned();
    }
    let (raw_len, out_len) = (raw.chars().count(), out.chars().count());
    if out.trim().is_empty() || out_len as f32 > 1.3 * raw_len as f32 + 10.0 || (has_loop(out) && !has_loop(raw)) {
        return raw.to_owned();
    }
    let a: Vec<&str> = raw.split_whitespace().collect();
    let b: Vec<&str> = out.split_whitespace().collect();
    let ca: Vec<String> = a.iter().map(|w| core(w)).collect();
    let cb: Vec<String> = b.iter().map(|w| core(w)).collect();

    let mut words: Vec<String> = Vec::with_capacity(a.len());
    // После отката (возвращённое слово, отброшенная вставка) регистр первой буквы
    // следующего слова берём из исходника: модель могла поднять или опустить его
    // из-за слова, которого в итоге нет.
    let mut reverted = false;
    let take = |word: &str, raw_word: Option<&str>, reverted: &mut bool, words: &mut Vec<String>| {
        let w = match raw_word {
            Some(r) if *reverted => with_case_of(r, word),
            _ => word.to_owned(),
        };
        *reverted = false;
        words.push(w);
    };
    for op in opcodes(&ca, &cb) {
        match op {
            Op::Equal(a1, a2, b1, b2) => {
                for (x, y) in a[a1..a2].iter().zip(&b[b1..b2]) {
                    take(y, Some(x), &mut reverted, &mut words);
                }
            }
            Op::Replace(a1, a2, b1, b2) if a2 - a1 <= 4 && b2 - b1 <= 4 && translit_ok(&a[a1..a2], &b[b1..b2]) => {
                for y in &b[b1..b2] {
                    take(y, None, &mut reverted, &mut words);
                }
            }
            Op::Replace(a1, a2, b1, b2) if a2 - a1 == b2 - b1 => {
                for (x, y) in a[a1..a2].iter().zip(&b[b1..b2]) {
                    if similar(x, y) {
                        take(y, Some(x), &mut reverted, &mut words);
                    } else {
                        words.push((*x).to_owned());
                        reverted = true;
                    }
                }
            }
            Op::Replace(a1, a2, _, _) => {
                words.extend(a[a1..a2].iter().map(|w| (*w).to_owned()));
                reverted = true;
            }
            Op::Delete(a1, a2) => {
                for i in a1..a2 {
                    let c = &ca[i];
                    let stutter = !c.is_empty()
                        && ((i > 0 && &ca[i - 1] == c) || (i + 1 < ca.len() && &ca[i + 1] == c));
                    if !stutter {
                        words.push(a[i].to_owned());
                        reverted = true;
                    }
                }
            }
            // Новые слова не принимаем; отдельное тире — можно.
            Op::Insert(b1, b2) => {
                for w in &b[b1..b2] {
                    if matches!(*w, "—" | "–") {
                        words.push((*w).to_owned());
                    } else {
                        reverted = true;
                    }
                }
            }
        }
    }
    let mut s = words.join(" ");

    let (_, raw_end) = split_end(raw.trim_end());
    let (body, end) = split_end(&s);
    // Модель может выдать знак отдельным словом («на .»): срезав его, срезаем и пробел.
    let body = body.trim_end();
    s = if !raw_end.is_empty() {
        format!("{body}{raw_end}")
    } else if !end.is_empty() && !end.contains('?') {
        body.to_owned()
    } else {
        s.clone()
    };
    for q in ['«', '»', '"'] {
        if !raw.contains(q) {
            s = s.replace(q, "");
        }
    }
    s
}

/// Слово модели, но с регистром первой буквы как в исходном слове.
fn with_case_of(raw: &str, word: &str) -> String {
    let raw_upper = raw.chars().find(|c| c.is_alphabetic()).map(char::is_uppercase);
    let mut done = false;
    word.chars()
        .map(|c| {
            if done || !c.is_alphabetic() {
                return c.to_string();
            }
            done = true;
            match raw_upper {
                Some(true) => c.to_uppercase().collect(),
                Some(false) => c.to_lowercase().collect(),
                None => c.to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::merge;

    #[test]
    fn keeps_punctuation_and_case_fixes() {
        assert_eq!(merge("Да обучай сколько нужно.", "Да, обучай, сколько нужно."), "Да, обучай, сколько нужно.");
        assert_eq!(merge("главное— это скорость.", "Главное — это скорость."), "Главное — это скорость.");
    }

    #[test]
    fn keeps_word_fixes_but_not_word_swaps() {
        assert_eq!(merge("Это еще работает.", "Это ещё работает."), "Это ещё работает.");
        assert_eq!(merge("Подключин в режиме отладки.", "Подключен в режиме отладки."), "Подключен в режиме отладки.");
        assert_eq!(merge("Книги размечены.", "Книги замечены."), "Книги размечены.");
        // Известное ограничение: «Переноси» → «Перенёс» без словаря неотличимо от
        // исправления окончания и проходит (та же первая буква, две правки).
    }

    #[test]
    fn restores_dropped_words_but_drops_stutter() {
        assert_eq!(merge("будет локально быстро работать.", "будет локально работать."), "будет локально быстро работать.");
        assert_eq!(merge("я я хотел сказать.", "Я хотел сказать."), "Я хотел сказать.");
        assert_eq!(merge("Это работает.", "А это работает."), "Это работает.");
        assert_eq!(merge("Тупо работает.", "Работает."), "Тупо работает.");
    }

    #[test]
    fn accepts_brand_transliteration() {
        assert_eq!(merge("Открой гитхаб.", "Открой GitHub."), "Открой GitHub.");
        assert_eq!(
            merge("Помогает дипсик четыре один флэш.", "Помогает DeepSeek 4.1 Flash."),
            "Помогает DeepSeek 4.1 Flash."
        );
        assert_eq!(merge("Спроси у клод.", "Спроси у Claude."), "Спроси у Claude.");
    }

    #[test]
    fn keeps_sentence_end_and_quotes() {
        assert_eq!(merge("модель хорошо работает?", "Модель хорошо работает."), "Модель хорошо работает?");
        assert_eq!(merge("меняли их на", "меняли их на."), "меняли их на");
        assert_eq!(merge("начал её обучать на", "начал её обучать на ."), "начал её обучать на");
        assert_eq!(merge("Вот так.", "Вот так ."), "Вот так.");
        assert_eq!(merge("Это работает также", "Это работает также?"), "Это работает также?");
        assert_eq!(merge("Буквы е-ё как контроль.", "Буквы е-ё как «контроль»."), "Буквы е-ё как контроль.");
    }

    #[test]
    fn rolls_back_loops_and_empty_output() {
        assert_eq!(merge("Телы.", "Телы.……Тела.……Тела.……"), "Телы.");
        assert_eq!(merge("Вот ключ.", "Вот ключ вот ключ вот ключ вот ключ."), "Вот ключ.");
        assert_eq!(merge("", "Для ситема три."), "");
        assert_eq!(merge("Привет.", ""), "Привет.");
    }
}
