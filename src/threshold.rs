#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateUnit {
    Second,
    Minute,
    Hour,
    Day,
}

impl RateUnit {
    pub fn milliseconds(self) -> u64 {
        match self {
            Self::Second => 1_000,
            Self::Minute => 60_000,
            Self::Hour => 3_600_000,
            Self::Day => 86_400_000,
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Self::Second => "s",
            Self::Minute => "m",
            Self::Hour => "h",
            Self::Day => "d",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "s" | "sec" | "second" | "seconds" => Some(Self::Second),
            "m" | "min" | "minute" | "minutes" => Some(Self::Minute),
            "h" | "hr" | "hour" | "hours" => Some(Self::Hour),
            "d" | "day" | "days" => Some(Self::Day),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowthThreshold {
    Absolute(u64),
    Rate { bytes_per_unit: u64, unit: RateUnit },
}

impl GrowthThreshold {
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if let Some((size, unit)) = value.split_once('/') {
            if unit.contains('/') {
                return Err(format!("无法识别增长速度：{value}"));
            }
            let unit =
                RateUnit::parse(unit.trim()).ok_or_else(|| format!("不支持的速度单位：{unit}"))?;
            let bytes_per_unit = parse_size_bytes(size)?;
            return Ok(Self::Rate {
                bytes_per_unit,
                unit,
            });
        }
        Ok(Self::Absolute(parse_size_bytes(value)?))
    }

    pub fn effective_bytes(self, elapsed_ms: u64) -> u64 {
        match self {
            Self::Absolute(bytes) => bytes,
            Self::Rate {
                bytes_per_unit,
                unit,
            } => {
                let bytes = u128::from(bytes_per_unit).saturating_mul(u128::from(elapsed_ms))
                    / u128::from(unit.milliseconds());
                bytes.min(u128::from(u64::MAX)) as u64
            }
        }
    }
}

pub fn parse_size_bytes(value: &str) -> Result<u64, String> {
    let value = value.trim().replace(',', "");
    if value.is_empty() {
        return Err("大小不能为空".to_string());
    }

    let split_at = value
        .find(|character: char| !(character.is_ascii_digit() || character == '.'))
        .unwrap_or(value.len());
    let (number_text, suffix) = value.split_at(split_at);
    let number = number_text
        .parse::<f64>()
        .map_err(|_| format!("无法识别大小：{value}"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("大小必须是非负数字：{value}"));
    }

    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1_f64,
        "k" | "kb" | "kib" => 1024_f64,
        "m" | "mb" | "mib" => 1024_f64.powi(2),
        "g" | "gb" | "gib" => 1024_f64.powi(3),
        "t" | "tb" | "tib" => 1024_f64.powi(4),
        _ => return Err(format!("不支持的大小单位：{suffix}")),
    };
    let bytes = number * multiplier;
    if bytes > u64::MAX as f64 {
        return Err(format!("大小超出范围：{value}"));
    }
    Ok(bytes.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::{GrowthThreshold, RateUnit, parse_size_bytes};

    #[test]
    fn parses_absolute_and_rate_thresholds() {
        assert_eq!(parse_size_bytes("1.5GiB"), Ok(1_610_612_736));
        assert_eq!(
            GrowthThreshold::parse("100M/d"),
            Ok(GrowthThreshold::Rate {
                bytes_per_unit: 100 * 1024 * 1024,
                unit: RateUnit::Day,
            })
        );
        assert_eq!(
            GrowthThreshold::parse("42"),
            Ok(GrowthThreshold::Absolute(42))
        );
    }

    #[test]
    fn converts_rate_to_elapsed_absolute_threshold() {
        let threshold = GrowthThreshold::parse("1M/s").expect("parse rate");
        assert_eq!(threshold.effective_bytes(1_000), 1024 * 1024);
        assert_eq!(threshold.effective_bytes(1_500), 1_572_864);
        assert_eq!(
            GrowthThreshold::parse("100M/d")
                .expect("parse daily rate")
                .effective_bytes(86_400_000),
            100 * 1024 * 1024
        );
    }

    #[test]
    fn rejects_invalid_rate_units() {
        assert!(GrowthThreshold::parse("1M/x").is_err());
        assert!(GrowthThreshold::parse("1M/d/h").is_err());
    }
}
