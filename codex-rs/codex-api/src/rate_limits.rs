use codex_protocol::protocol::CreditsSnapshot;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow;
use http::HeaderMap;
use std::fmt::Display;

#[derive(Debug)]
pub struct RateLimitError {
    pub message: String,
}

impl Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Parses the bespoke Codex rate-limit headers into a `RateLimitSnapshot`.
pub fn parse_rate_limit(headers: &HeaderMap) -> Option<RateLimitSnapshot> {
    let primary = parse_rate_limit_window(
        headers,
        "x-codex-primary-used-percent",
        "x-codex-primary-window-minutes",
        "x-codex-primary-reset-at",
    );

    let secondary = parse_rate_limit_window(
        headers,
        "x-codex-secondary-used-percent",
        "x-codex-secondary-window-minutes",
        "x-codex-secondary-reset-at",
    );

    let credits = parse_credits_snapshot(headers);

    Some(RateLimitSnapshot {
        primary,
        secondary,
        credits,
        plan_type: None,
    })
}

fn parse_rate_limit_window(
    headers: &HeaderMap,
    used_percent_header: &str,
    window_minutes_header: &str,
    resets_at_header: &str,
) -> Option<RateLimitWindow> {
    let used_percent: Option<f64> = parse_header_f64(headers, used_percent_header);

    used_percent.and_then(|used_percent| {
        let window_minutes = parse_header_i64(headers, window_minutes_header);
        let resets_at = parse_header_i64(headers, resets_at_header);

        let has_data = used_percent != 0.0
            || window_minutes.is_some_and(|minutes| minutes != 0)
            || resets_at.is_some();

        has_data.then_some(RateLimitWindow {
            used_percent,
            window_minutes,
            resets_at,
        })
    })
}

fn parse_credits_snapshot(headers: &HeaderMap) -> Option<CreditsSnapshot> {
    let has_credits = parse_header_bool(headers, "x-codex-credits-has-credits")?;
    let unlimited = parse_header_bool(headers, "x-codex-credits-unlimited")?;
    let balance = parse_header_str(headers, "x-codex-credits-balance")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::string::ToString::to_string);
    Some(CreditsSnapshot {
        has_credits,
        unlimited,
        balance,
    })
}

fn parse_header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    parse_header_str(headers, name)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

fn parse_header_i64(headers: &HeaderMap, name: &str) -> Option<i64> {
    parse_header_str(headers, name)?.parse::<i64>().ok()
}

fn parse_header_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    let raw = parse_header_str(headers, name)?;
    if raw.eq_ignore_ascii_case("true") || raw == "1" {
        Some(true)
    } else if raw.eq_ignore_ascii_case("false") || raw == "0" {
        Some(false)
    } else {
        None
    }
}

fn parse_header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// Parses Anthropic rate-limit headers into a `RateLimitSnapshot`.
///
/// Anthropic headers:
/// - `anthropic-ratelimit-requests-limit` / `anthropic-ratelimit-requests-remaining`
/// - `anthropic-ratelimit-tokens-limit` / `anthropic-ratelimit-tokens-remaining`
///
/// Note: Reset timestamps are not parsed to avoid adding datetime dependencies.
pub fn parse_anthropic_rate_limit(headers: &HeaderMap) -> Option<RateLimitSnapshot> {
    // Requests rate limit (primary)
    let primary = parse_anthropic_rate_limit_window(
        headers,
        "anthropic-ratelimit-requests-limit",
        "anthropic-ratelimit-requests-remaining",
    );

    // Tokens rate limit (secondary)
    let secondary = parse_anthropic_rate_limit_window(
        headers,
        "anthropic-ratelimit-tokens-limit",
        "anthropic-ratelimit-tokens-remaining",
    );

    if primary.is_none() && secondary.is_none() {
        return None;
    }

    Some(RateLimitSnapshot {
        primary,
        secondary,
        credits: None,
        plan_type: None,
    })
}

fn parse_anthropic_rate_limit_window(
    headers: &HeaderMap,
    limit_header: &str,
    remaining_header: &str,
) -> Option<RateLimitWindow> {
    let limit = parse_header_i64(headers, limit_header)?;
    let remaining = parse_header_i64(headers, remaining_header)?;

    // Calculate used percent: (limit - remaining) / limit * 100
    let used_percent = if limit > 0 {
        ((limit - remaining) as f64 / limit as f64) * 100.0
    } else {
        0.0
    };

    Some(RateLimitWindow {
        used_percent,
        window_minutes: None, // Anthropic doesn't provide window duration
        resets_at: None,      // Skip parsing ISO 8601 to avoid datetime deps
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;
    use http::HeaderValue;

    #[test]
    fn parse_anthropic_rate_limit_with_requests_and_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-requests-limit",
            HeaderValue::from_static("100"),
        );
        headers.insert(
            "anthropic-ratelimit-requests-remaining",
            HeaderValue::from_static("75"),
        );
        headers.insert(
            "anthropic-ratelimit-tokens-limit",
            HeaderValue::from_static("10000"),
        );
        headers.insert(
            "anthropic-ratelimit-tokens-remaining",
            HeaderValue::from_static("8000"),
        );

        let snapshot = parse_anthropic_rate_limit(&headers).expect("should parse");

        // Primary (requests): 25% used (100 - 75) / 100
        let primary = snapshot.primary.expect("primary window");
        assert!((primary.used_percent - 25.0).abs() < 0.01);

        // Secondary (tokens): 20% used (10000 - 8000) / 10000
        let secondary = snapshot.secondary.expect("secondary window");
        assert!((secondary.used_percent - 20.0).abs() < 0.01);
    }

    #[test]
    fn parse_anthropic_rate_limit_missing_headers_returns_none() {
        let headers = HeaderMap::new();
        assert!(parse_anthropic_rate_limit(&headers).is_none());
    }
}
