//! Provider waits recognized from live status controls, never transcript prose.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderLimit {
    pub retry_after_ms: Option<u64>,
}

pub(crate) fn opencode(screen: &str) -> Option<ProviderLimit> {
    let lines: Vec<_> = screen.lines().rev().take(40).collect();
    // The captured status row wraps in narrow panes. Only a progress run may
    // start a candidate, and only its next three nonblank rows may continue it.
    let (index, line) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| has_progress_prefix(line.trim_start()))?;
    let mut row = line.trim_start().to_string();
    for offset in 0..4 {
        if offset != 0 {
            let Some(next) = index.checked_sub(offset).map(|i| lines[i]) else {
                break;
            };
            if next.trim().is_empty() || has_progress_prefix(next.trim_start()) {
                break;
            }
            row.push_str(next);
        }
        let lower: String = row
            .to_lowercase()
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        let error = lower.contains("usageexceeded")
            || lower.contains("usagelimitreached")
            || lower.contains("ratelimit");
        let interrupt = lower.contains("escinterrupt") || lower.contains("escagaintointerrupt");
        if error && interrupt && lower.contains("attempt#") {
            if let Some((_, retry)) = lower.split_once("retryingin") {
                if let Some((eta, _)) = retry.split_once("attempt#") {
                    return Some(ProviderLimit {
                        retry_after_ms: parse_duration(eta).ok(),
                    });
                }
            }
        }
        if interrupt {
            break;
        }
    }
    None
}

fn has_progress_prefix(line: &str) -> bool {
    line.chars()
        .take_while(|ch| matches!(ch, '■' | '⬝'))
        .count()
        >= 4
}

pub(crate) fn parse_duration(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value == "0" {
        return Ok(0);
    }
    let mut total = 0_u64;
    let mut rest = value;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let count = rest.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 {
            return Err(format!("invalid duration: {value}"));
        }
        let number = rest[..count]
            .parse::<u64>()
            .map_err(|_| format!("invalid duration: {value}"))?;
        rest = &rest[count..];
        let (unit, factor) = if rest.starts_with("ms") {
            (2, 1)
        } else if rest.starts_with('s') {
            (1, 1_000)
        } else if rest.starts_with('m') {
            (1, 60_000)
        } else if rest.starts_with('h') {
            (1, 3_600_000)
        } else {
            return Err(format!("duration needs ms, s, m or h: {value}"));
        };
        total = number
            .checked_mul(factor)
            .and_then(|n| total.checked_add(n))
            .ok_or_else(|| format!("duration too large: {value}"))?;
        rest = &rest[unit..];
    }
    if value.is_empty() {
        return Err("duration is empty".into());
    }
    Ok(total)
}
