#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingTarget {
    Implicit,
    Ordinal(usize),
    Token(String),
    Last,
}

pub fn parse_pending_target(text: &str) -> PendingTarget {
    let normalized = normalize_pending_target_text(text);
    if normalized.is_empty() {
        return PendingTarget::Implicit;
    }

    if normalized.eq_ignore_ascii_case("last") || normalized.eq_ignore_ascii_case("latest") {
        return PendingTarget::Last;
    }

    if let Ok(index) = normalized.parse::<usize>() {
        if index > 0 {
            return PendingTarget::Ordinal(index);
        }
    }

    if let Some(index) = parse_chinese_ordinal(normalized) {
        return PendingTarget::Ordinal(index);
    }

    PendingTarget::Token(normalized.to_owned())
}

fn normalize_pending_target_text(text: &str) -> &str {
    text.trim()
        .trim_end_matches(|c: char| matches!(c, '.' | ')' | ',' | ':' | ';'))
        .trim()
}

fn parse_chinese_ordinal(text: &str) -> Option<usize> {
    let candidate = text
        .strip_prefix('\u{7b2c}')
        .unwrap_or(text)
        .strip_suffix("\u{9879}")
        .or_else(|| {
            text.strip_prefix('\u{7b2c}')
                .unwrap_or(text)
                .strip_suffix('\u{4e2a}')
        })
        .or_else(|| {
            text.strip_prefix('\u{7b2c}')
                .unwrap_or(text)
                .strip_suffix('\u{6761}')
        })
        .or_else(|| {
            text.strip_prefix('\u{7b2c}')
                .unwrap_or(text)
                .strip_suffix('\u{9898}')
        })
        .unwrap_or(text.strip_prefix('\u{7b2c}').unwrap_or(text));

    if let Ok(index) = candidate.parse::<usize>() {
        return (index > 0).then_some(index);
    }

    parse_simple_chinese_number(candidate)
}

fn parse_simple_chinese_number(text: &str) -> Option<usize> {
    if text.is_empty() {
        return None;
    }
    if text == "\u{5341}" {
        return Some(10);
    }
    if let Some(ones) = text.strip_prefix('\u{5341}') {
        return chinese_digit_value(ones).map(|ones| 10 + ones);
    }
    if let Some(tens) = text.strip_suffix('\u{5341}') {
        return chinese_digit_value(tens).map(|tens| tens * 10);
    }
    if let Some((tens, ones)) = text.split_once('\u{5341}') {
        return Some(chinese_digit_value(tens)? * 10 + chinese_digit_value(ones)?);
    }
    chinese_digit_value(text)
}

fn chinese_digit_value(text: &str) -> Option<usize> {
    match text {
        "\u{4e00}" => Some(1),
        "\u{4e8c}" | "\u{4e24}" => Some(2),
        "\u{4e09}" => Some(3),
        "\u{56db}" => Some(4),
        "\u{4e94}" => Some(5),
        "\u{516d}" => Some(6),
        "\u{4e03}" => Some(7),
        "\u{516b}" => Some(8),
        "\u{4e5d}" => Some(9),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_pending_target, PendingTarget};

    #[test]
    fn parses_explicit_and_implicit_targets_in_domain_layer() {
        assert_eq!(parse_pending_target("1"), PendingTarget::Ordinal(1));
        assert_eq!(
            parse_pending_target("req-7"),
            PendingTarget::Token("req-7".to_owned())
        );
        assert_eq!(parse_pending_target("last"), PendingTarget::Last);
    }

    #[test]
    fn parses_chinese_ordinals() {
        assert_eq!(parse_pending_target("第2项"), PendingTarget::Ordinal(2));
        assert_eq!(parse_pending_target("第三个"), PendingTarget::Ordinal(3));
    }
}
