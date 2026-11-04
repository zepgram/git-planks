use anyhow::{Result, bail};
use std::time::{SystemTime, UNIX_EPOCH};

const SECONDS_PER_DAY: i64 = 86_400;
const FIRST_MONDAY: i64 = 4;

pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn parse_date(text: &str) -> Result<i64> {
    let parts: Vec<&str> = text.split('-').collect();
    if let [year, month, day] = parts[..]
        && let (Ok(year), Ok(month), Ok(day)) = (year.parse(), month.parse(), day.parse())
    {
        let days = days_from_civil(year, month, day);
        if civil_from_days(days) == (year, month, day) {
            return Ok(days);
        }
    }
    bail!("invalid date `{text}`, expected YYYY-MM-DD")
}

pub fn format_date(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn today() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    seconds.div_euclid(SECONDS_PER_DAY)
}

pub fn day_of(timestamp: i64) -> i64 {
    timestamp.div_euclid(SECONDS_PER_DAY)
}

pub fn start_of_day(days: i64) -> i64 {
    days * SECONDS_PER_DAY
}

pub fn end_of_day(days: i64) -> i64 {
    days * SECONDS_PER_DAY + SECONDS_PER_DAY - 1
}

pub fn quarter_of(timestamp: i64) -> String {
    let (year, month, _) = civil_from_days(day_of(timestamp));
    format!("{year}Q{}", (month - 1) / 3 + 1)
}

pub fn month_of(timestamp: i64) -> String {
    let (year, month, _) = civil_from_days(day_of(timestamp));
    format!("{year}-{month:02}")
}

pub fn sample_dates(start: i64, until: i64, interval_weeks: i64) -> Vec<i64> {
    let step = 7 * interval_weeks.max(1);
    let mut current = (start - FIRST_MONDAY).div_euclid(step) * step + FIRST_MONDAY;
    let mut dates = Vec::new();
    while current < until {
        dates.push(current);
        current += step;
    }
    dates.push(until);
    dates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_civil_dates_both_ways() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(format_date(parse_date("2024-02-29").unwrap()), "2024-02-29");
    }

    #[test]
    fn rejects_impossible_dates() {
        assert!(parse_date("2025-02-29").is_err());
        assert!(parse_date("not-a-date").is_err());
    }

    #[test]
    fn samples_on_mondays_and_ends_on_the_cutoff() {
        let dates = sample_dates(
            parse_date("2024-01-03").unwrap(),
            parse_date("2024-01-17").unwrap(),
            1,
        );
        let labels: Vec<String> = dates.iter().map(|day| format_date(*day)).collect();
        assert_eq!(
            labels,
            ["2024-01-01", "2024-01-08", "2024-01-15", "2024-01-17"]
        );
    }

    #[test]
    fn keeps_a_stable_grid_for_wider_intervals() {
        let late_start = sample_dates(
            parse_date("2024-01-08").unwrap(),
            parse_date("2024-03-04").unwrap(),
            4,
        );
        let early_start = sample_dates(
            parse_date("2023-06-01").unwrap(),
            parse_date("2024-03-04").unwrap(),
            4,
        );
        assert!(late_start.iter().all(|date| early_start.contains(date)));
    }

    #[test]
    fn buckets_timestamps_in_utc() {
        let timestamp = start_of_day(parse_date("2025-11-30").unwrap());
        assert_eq!(quarter_of(timestamp), "2025Q4");
        assert_eq!(month_of(timestamp), "2025-11");
        assert_eq!(end_of_day(0), 86_399);
    }
}
