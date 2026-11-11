const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

pub fn bytes(count: u64) -> String {
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{count} B"),
        _ if value >= 100.0 => format!("{value:.0} {}", UNITS[unit]),
        _ if value >= 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.2} {}", UNITS[unit]),
    }
}

pub fn age(seconds: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    match seconds {
        ..MINUTE => "under 1m".to_string(),
        MINUTE..HOUR => format!("{}m", seconds / MINUTE),
        HOUR..DAY => format!("{}h", seconds / HOUR),
        _ if seconds < 60 * DAY => format!("{}d", seconds / DAY),
        _ if seconds < 730 * DAY => format!("{}mo", seconds / (30 * DAY)),
        _ => format!("{}y", seconds / (365 * DAY)),
    }
}

pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_keep_three_significant_digits() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.50 KB");
        assert_eq!(bytes(12 * 1024 * 1024 + 300 * 1024), "12.3 MB");
        assert_eq!(bytes(107_374_182_400), "100 GB");
    }

    #[test]
    fn ages_use_the_largest_fitting_unit() {
        assert_eq!(age(30), "under 1m");
        assert_eq!(age(5 * 3600), "5h");
        assert_eq!(age(9 * 86400), "9d");
        assert_eq!(age(90 * 86400), "3mo");
    }

    #[test]
    fn counts_are_grouped_by_thousands() {
        assert_eq!(count(999), "999");
        assert_eq!(count(1_234_567), "1,234,567");
    }
}
