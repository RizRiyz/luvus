use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn rfc3339_millis(time: SystemTime) -> String {
    let nanos = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            i128::from(duration.as_secs()) * 1_000_000_000 + i128::from(duration.subsec_nanos())
        }
        Err(error) => {
            let duration = error.duration();
            -(i128::from(duration.as_secs()) * 1_000_000_000 + i128::from(duration.subsec_nanos()))
        }
    };
    let millis = if nanos >= 0 {
        (nanos + 500_000) / 1_000_000
    } else {
        -((-nanos + 500_000) / 1_000_000)
    };
    let days = millis.div_euclid(86_400_000);
    let day_millis = millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = day_millis / 3_600_000;
    let minute = day_millis % 3_600_000 / 60_000;
    let second = day_millis % 60_000 / 1_000;
    let millisecond = day_millis % 1_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z")
}

fn civil_from_days(days_since_epoch: i128) -> (i128, i128, i128) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i128::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_epoch_and_milliseconds() {
        assert_eq!(rfc3339_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_millis(UNIX_EPOCH + Duration::from_millis(1_234)),
            "1970-01-01T00:00:01.234Z"
        );
        assert_eq!(
            rfc3339_millis(UNIX_EPOCH + Duration::from_nanos(1_234_600_000)),
            "1970-01-01T00:00:01.235Z"
        );
    }

    #[test]
    fn formats_before_epoch_and_leap_day() {
        assert_eq!(
            rfc3339_millis(UNIX_EPOCH - Duration::from_millis(1)),
            "1969-12-31T23:59:59.999Z"
        );
        assert_eq!(
            rfc3339_millis(UNIX_EPOCH + Duration::from_secs(1_582_934_400)),
            "2020-02-29T00:00:00.000Z"
        );
    }
}
