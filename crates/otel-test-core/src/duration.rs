use std::time::Duration;

pub(crate) fn parse(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, unit) = s.split_at(split);
    if digits.is_empty() {
        return Err(format!(
            "invalid duration {input:?}: expected an integer followed by a unit, e.g. \"500ms\", \"3s\" or \"1m\""
        ));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration {input:?}: number is too large"))?;
    let duration = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => n
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| format!("invalid duration {input:?}: number is too large"))?,
        "" => {
            return Err(format!(
                "invalid duration {input:?}: missing unit (use ms, s or m)"
            ));
        }
        other => {
            return Err(format!(
                "invalid duration {input:?}: unknown unit {other:?} (use ms, s or m)"
            ));
        }
    };
    Ok(duration)
}

pub(crate) fn parse_go(input: &str) -> Option<Duration> {
    let s = input.trim();
    if s == "0" {
        return Some(Duration::ZERO);
    }
    let mut rest = s;
    let mut total = 0f64;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let (num, tail) = rest.split_at(num_end);
        let value: f64 = num.parse().ok()?;
        let unit_end = tail
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(tail.len());
        let (unit, tail) = tail.split_at(unit_end);
        let scale = match unit {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        total += value * scale;
        rest = tail;
    }
    Duration::try_from_secs_f64(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_units() {
        assert_eq!(parse("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse("3s"), Ok(Duration::from_secs(3)));
        assert_eq!(parse(" 2m "), Ok(Duration::from_secs(120)));
        assert_eq!(parse("0s"), Ok(Duration::ZERO));
    }

    #[test]
    fn parses_go_durations() {
        assert_eq!(parse_go("200ms"), Some(Duration::from_millis(200)));
        assert_eq!(parse_go("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_go("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_go("0"), Some(Duration::ZERO));
        assert_eq!(parse_go("500us"), Some(Duration::from_micros(500)));
        for bad in ["", "5", "s", "5x", "1m30", "-1s", "${env:T}"] {
            assert_eq!(parse_go(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "",
            "s",
            "5",
            "1.5s",
            "5h",
            "-1s",
            "5 s",
            "99999999999999999999s",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(parse("5").unwrap_err().contains("missing unit"));
        assert!(parse("5h").unwrap_err().contains("unknown unit \"h\""));
    }
}
