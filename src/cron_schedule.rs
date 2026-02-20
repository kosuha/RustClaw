use std::str::FromStr;

use cron::Schedule;

pub fn parse_cron_schedule(schedule_value: &str) -> Result<Schedule, String> {
    let trimmed = schedule_value.trim();
    let normalized = if trimmed.split_whitespace().count() == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    };

    Schedule::from_str(&normalized).map_err(|err| format!("invalid cron expression: {err}"))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn parse_cron_schedule_accepts_five_field_expression() {
        let schedule = parse_cron_schedule("0 9 * * *").expect("parse cron");
        let now = Utc.with_ymd_and_hms(2026, 2, 19, 0, 0, 0).unwrap();
        let next_run = schedule.after(&now).next().expect("next run");

        assert_eq!(
            next_run,
            Utc.with_ymd_and_hms(2026, 2, 19, 9, 0, 0).unwrap()
        );
    }

    #[test]
    fn parse_cron_schedule_accepts_six_field_expression() {
        parse_cron_schedule("0 0 9 * * *").expect("parse six-field cron");
    }
}
