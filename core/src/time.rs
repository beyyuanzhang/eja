use crate::{Error, Result};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

/// Physical wall-clock instant stored as Unix seconds and serialized as RFC 3339 UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn now() -> Self {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => Self(duration.as_secs().min(i64::MAX as u64) as i64),
            Err(err) => Self(-(err.duration().as_secs().min(i64::MAX as u64) as i64)),
        }
    }

    pub const fn from_unix(seconds: i64) -> Self {
        Self(seconds)
    }

    pub const fn as_unix(self) -> i64 {
        self.0
    }

    pub fn parse_rfc3339(text: &str) -> Result<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 20
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || bytes[10] != b'T'
            || bytes[13] != b':'
            || bytes[16] != b':'
            || bytes[19] != b'Z'
        {
            return Err(Error::invalid("expected RFC 3339 UTC timestamp"));
        }
        let year = parse_digits(&bytes[0..4])?;
        let month = parse_digits(&bytes[5..7])?;
        let day = parse_digits(&bytes[8..10])?;
        let hour = parse_digits(&bytes[11..13])?;
        let minute = parse_digits(&bytes[14..16])?;
        let second = parse_digits(&bytes[17..19])?;
        if !(1..=12).contains(&month)
            || !(1..=days_in_month(year, month)).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return Err(Error::invalid("invalid calendar date or time"));
        }
        let adjusted_year = year - i64::from(month <= 2);
        let era = adjusted_year.div_euclid(400);
        let year_of_era = adjusted_year - era * 400;
        let march_month = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * march_month + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;
        Ok(Self(days * 86_400 + hour * 3_600 + minute * 60 + second))
    }
}

fn parse_digits(slice: &[u8]) -> Result<i64> {
    let mut value = 0i64;
    for &byte in slice {
        if !byte.is_ascii_digit() {
            return Err(Error::invalid("non-digit in timestamp"));
        }
        value = value * 10 + i64::from(byte - b'0');
    }
    Ok(value)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(86_400);
        let rem = self.0.rem_euclid(86_400);
        let hour = rem / 3_600;
        let minute = (rem % 3_600) / 60;
        let second = rem % 60;

        let shifted = days + 719_468;
        let era = shifted.div_euclid(146_097);
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let base_year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let march_month = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * march_month + 2) / 5 + 1;
        let month = if march_month < 10 {
            march_month + 3
        } else {
            march_month - 9
        };
        let year = base_year + i64::from(month <= 2);
        write!(
            formatter,
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
        )
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = Timestamp;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("RFC 3339 UTC timestamp string or Unix seconds")
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(Timestamp(value))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Self::Value, E> {
                i64::try_from(value)
                    .map(Timestamp)
                    .map_err(|_| E::custom("timestamp out of range"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Timestamp::parse_rfc3339(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}
