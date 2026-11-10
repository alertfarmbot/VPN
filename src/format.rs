//! Чистые функции оформления текста: время, подписи, инициалы.

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::backend::MessageData;

const WEEKDAYS: [&str; 7] = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"];
const MONTHS: [&str; 12] = [
    "января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября",
    "октября", "ноября", "декабря",
];

/// Перевод unix-времени в местное.
pub struct Clock {
    tz: TimeZone,
}

struct Local {
    day: i64,
    year: i16,
    month: i8,
    day_of_month: i8,
    weekday: usize,
    hour: i8,
    minute: i8,
}

impl Clock {
    pub fn system() -> Self {
        Self::new(TimeZone::system())
    }

    pub fn new(tz: TimeZone) -> Self {
        Self { tz }
    }

    pub fn now() -> i64 {
        Timestamp::now().as_second()
    }

    fn local(&self, ts: i64) -> Local {
        let zoned = Timestamp::from_second(ts).unwrap_or(Timestamp::UNIX_EPOCH).to_zoned(self.tz.clone());
        Local {
            day: (ts + i64::from(zoned.offset().seconds())).div_euclid(86_400),
            year: zoned.year(),
            month: zoned.month(),
            day_of_month: zoned.day(),
            weekday: zoned.weekday().to_monday_zero_offset() as usize,
            hour: zoned.hour(),
            minute: zoned.minute(),
        }
    }

    /// Номер местных суток: одинаков у меток времени одного дня.
    pub fn day(&self, ts: i64) -> i64 {
        self.local(ts).day
    }

    pub fn time(&self, ts: i64) -> String {
        let local = self.local(ts);
        format!("{:02}:{:02}", local.hour, local.minute)
    }

    /// Метка в списке чатов: время сегодня, день недели на этой неделе, иначе дата.
    pub fn list_stamp(&self, ts: i64, now: i64) -> String {
        let local = self.local(ts);
        match self.day(now) - local.day {
            0 => format!("{:02}:{:02}", local.hour, local.minute),
            1..=6 => WEEKDAYS[local.weekday].to_owned(),
            _ => short_date(&local),
        }
    }

    /// Заголовок-разделитель дней в переписке.
    pub fn day_label(&self, ts: i64, now: i64) -> String {
        let local = self.local(ts);
        let today = self.local(now);
        let month = MONTHS[(local.month - 1) as usize];
        match today.day - local.day {
            0 => "Сегодня".to_owned(),
            1 => "Вчера".to_owned(),
            _ if local.year == today.year => format!("{} {month}", local.day_of_month),
            _ => format!("{} {month} {}", local.day_of_month, local.year),
        }
    }

    pub fn last_seen(&self, ts: i64, now: i64) -> String {
        let local = self.local(ts);
        match self.day(now) - local.day {
            0 => format!("был(а) в {:02}:{:02}", local.hour, local.minute),
            1 => format!("был(а) вчера в {:02}:{:02}", local.hour, local.minute),
            _ => format!("был(а) {}", short_date(&local)),
        }
    }
}

fn short_date(local: &Local) -> String {
    format!("{:02}.{:02}.{:02}", local.day_of_month, local.month, local.year.rem_euclid(100))
}

/// До двух заглавных букв для аватара без фотографии.
pub fn initials(title: &str) -> String {
    let mut out = String::new();
    for word in title.split_whitespace() {
        if let Some(letter) = word.chars().find(|ch| ch.is_alphanumeric()) {
            out.extend(letter.to_uppercase());
            if out.chars().count() >= 2 {
                break;
            }
        }
    }
    if out.is_empty() {
        out.extend(title.chars().find(|ch| !ch.is_whitespace()));
    }
    out
}

/// Номер цвета аватара и имени отправителя; палитра из семи цветов.
pub fn color_index(id: i64) -> i32 {
    (id.unsigned_abs() % 7) as i32
}

/// Выбирает форму слова для числа: 1 чат, 2 чата, 5 чатов.
pub fn plural<'a>(n: i64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let n = n.abs();
    match (n % 10, n % 100) {
        (_, 11..=14) => many,
        (1, _) => one,
        (2..=4, _) => few,
        _ => many,
    }
}

/// Число с неразрывными пробелами между разрядами: 12 840.
pub fn count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::from(if n < 0 { "-" } else { "" });
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push('\u{a0}');
        }
        out.push(digit);
    }
    out
}

/// Текст в одну строку не длиннее `max` символов, для списка чатов.
pub fn single_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        if count > 0 {
            out.push(' ');
            count += 1;
        }
        for ch in word.chars() {
            if count >= max {
                out.push('…');
                return out;
            }
            out.push(ch);
            count += 1;
        }
    }
    out
}

/// Строка предпросмотра сообщения в списке чатов.
pub fn preview(message: &MessageData) -> String {
    let text = single_line(&message.text, 120);
    match (message.media.is_empty(), text.is_empty()) {
        (true, _) => text,
        (false, true) => message.media.clone(),
        (false, false) => format!("{}, {text}", message.media),
    }
}

/// Делит текст на части не длиннее `limit` единиц UTF-16, по возможности по пробелам.
/// Telegram не принимает сообщения длиннее 4096 единиц.
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    fn push(parts: &mut Vec<String>, piece: &str) {
        let piece = piece.trim();
        if !piece.is_empty() {
            parts.push(piece.to_owned());
        }
    }

    let mut parts = Vec::new();
    let mut start = 0;
    let mut units = 0;
    // Место после последнего пробела в текущей части и её длина до него.
    let mut soft: Option<(usize, usize)> = None;
    for (index, ch) in text.char_indices() {
        let width = ch.len_utf16();
        if units + width > limit {
            let (cut, used) = soft.filter(|(cut, _)| *cut > start).unwrap_or((index, units));
            push(&mut parts, &text[start..cut]);
            start = cut;
            units -= used;
            soft = None;
        }
        units += width;
        if ch.is_whitespace() {
            soft = Some((index + ch.len_utf8(), units));
        }
    }
    push(&mut parts, &text[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> Clock {
        Clock::new(TimeZone::fixed(jiff::tz::offset(3)))
    }

    /// 2024-03-12 12:30:00 UTC, вторник.
    const NOON: i64 = 1_710_246_600;
    const DAY: i64 = 86_400;

    #[test]
    fn time_uses_local_zone() {
        assert_eq!(clock().time(NOON), "15:30");
        assert_eq!(Clock::new(TimeZone::UTC).time(NOON), "12:30");
    }

    #[test]
    fn day_changes_at_local_midnight() {
        let clock = clock();
        // 20:59:59 UTC — ещё тот же день по UTC+3, секундой позже — уже следующий.
        let before = NOON - 12 * 3600 - 1800 + 21 * 3600 - 1;
        assert_eq!(clock.day(before), clock.day(NOON));
        assert_eq!(clock.day(before + 1), clock.day(NOON) + 1);
    }

    #[test]
    fn list_stamp_gets_coarser_with_age() {
        let clock = clock();
        assert_eq!(clock.list_stamp(NOON, NOON + 3600), "15:30");
        assert_eq!(clock.list_stamp(NOON, NOON + DAY), "Вт");
        assert_eq!(clock.list_stamp(NOON, NOON + 6 * DAY), "Вт");
        assert_eq!(clock.list_stamp(NOON, NOON + 7 * DAY), "12.03.24");
    }

    #[test]
    fn day_label_names_recent_days() {
        let clock = clock();
        assert_eq!(clock.day_label(NOON, NOON), "Сегодня");
        assert_eq!(clock.day_label(NOON, NOON + DAY), "Вчера");
        assert_eq!(clock.day_label(NOON, NOON + 30 * DAY), "12 марта");
        assert_eq!(clock.day_label(NOON, NOON + 400 * DAY), "12 марта 2024");
    }

    #[test]
    fn last_seen_wording() {
        let clock = clock();
        assert_eq!(clock.last_seen(NOON, NOON + 60), "был(а) в 15:30");
        assert_eq!(clock.last_seen(NOON, NOON + DAY), "был(а) вчера в 15:30");
        assert_eq!(clock.last_seen(NOON, NOON + 3 * DAY), "был(а) 12.03.24");
    }

    #[test]
    fn initials_take_first_letters() {
        assert_eq!(initials("Анна Каренина"), "АК");
        assert_eq!(initials("rust lang chat"), "RL");
        assert_eq!(initials("  Одиночка "), "О");
        assert_eq!(initials("«Клуб» №5"), "К5");
        assert_eq!(initials("🙂"), "🙂");
        assert_eq!(initials(""), "");
    }

    #[test]
    fn color_index_is_stable_and_in_range() {
        for id in [0, 1, -1, 777, -1_001_234_567_890, i64::MIN, i64::MAX] {
            assert!((0..7).contains(&color_index(id)));
        }
        assert_eq!(color_index(10), color_index(-10));
    }

    #[test]
    fn plural_forms() {
        let form = |n| plural(n, "участник", "участника", "участников");
        assert_eq!(form(1), "участник");
        assert_eq!(form(3), "участника");
        assert_eq!(form(5), "участников");
        assert_eq!(form(11), "участников");
        assert_eq!(form(21), "участник");
        assert_eq!(form(112), "участников");
        assert_eq!(form(1002), "участника");
    }

    #[test]
    fn count_groups_digits() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1\u{a0}000");
        assert_eq!(count(12_840), "12\u{a0}840");
        assert_eq!(count(-1_234_567), "-1\u{a0}234\u{a0}567");
    }

    #[test]
    fn single_line_collapses_and_truncates() {
        assert_eq!(single_line("  привет,\n\nмир\t!  ", 100), "привет, мир !");
        assert_eq!(single_line("абвгдеж", 3), "абв…");
        assert_eq!(single_line("аб вг", 3), "аб …");
        assert_eq!(single_line("", 3), "");
    }

    #[test]
    fn preview_combines_media_and_text() {
        let mut message = MessageData {
            id: 1,
            chat: 1,
            date: 0,
            outgoing: false,
            sender_id: 1,
            sender: String::new(),
            text: "подпись\nв две строки".into(),
            media: String::new(),
            service: false,
            edited: false,
        };
        assert_eq!(preview(&message), "подпись в две строки");
        message.media = "Фото".into();
        assert_eq!(preview(&message), "Фото, подпись в две строки");
        message.text.clear();
        assert_eq!(preview(&message), "Фото");
    }

    #[test]
    fn split_keeps_short_text_whole() {
        assert_eq!(split_message("  привет  ", 4096), vec!["привет"]);
        assert!(split_message("   ", 4096).is_empty());
    }

    #[test]
    fn split_prefers_whitespace_and_respects_limit() {
        let parts = split_message("aaa bbb ccc ddd", 8);
        assert_eq!(parts, vec!["aaa bbb", "ccc ddd"]);

        let parts = split_message("abcdefghij", 4);
        assert_eq!(parts, vec!["abcd", "efgh", "ij"]);

        // Эмодзи занимает две единицы UTF-16.
        let parts = split_message("🙂🙂🙂", 4);
        assert_eq!(parts, vec!["🙂🙂", "🙂"]);
    }

    #[test]
    fn split_loses_nothing_but_whitespace() {
        let text = "слово ".repeat(3000);
        let parts = split_message(&text, 4096);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(part.encode_utf16().count() <= 4096);
        }
        assert_eq!(parts.join(" "), text.trim());
    }
}
