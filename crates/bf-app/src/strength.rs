//! Password rules and a length + character-class strength heuristic, 0–4
//! (spec §3.2; not zxcvbn — matches mobile [PARTIAL]).

pub const MIN_PASSWORD_LEN: usize = 10;

pub fn strength(pw: &str) -> u8 {
    let n = pw.chars().count();
    if n == 0 {
        return 0;
    }
    let classes = [
        pw.chars().any(|c| c.is_lowercase()),
        pw.chars().any(|c| c.is_uppercase()),
        pw.chars().any(|c| c.is_ascii_digit()),
        pw.chars().any(|c| !c.is_alphanumeric()),
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    let mut score: i32 = match n {
        0..=7 => 0,
        8..=9 => 1,
        10..=13 => 2,
        14..=19 => 3,
        _ => 4,
    };
    score += classes as i32 - 2;
    score.clamp(0, 4) as u8
}

pub fn strength_label(s: u8) -> &'static str {
    ["Very weak", "Weak", "Fair", "Strong", "Very strong"][s.min(4) as usize]
}

/// Validate the onboarding pair. Returns a human sentence on failure.
pub fn validate_pair(login: &str, login2: &str, destruction: &str, destruction2: &str) -> Result<(), String> {
    if login.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!("The password must be at least {MIN_PASSWORD_LEN} characters."));
    }
    if login != login2 {
        return Err("The passwords don't match.".into());
    }
    if destruction.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!("The second password must be at least {MIN_PASSWORD_LEN} characters."));
    }
    if destruction != destruction2 {
        return Err("The second passwords don't match.".into());
    }
    if login == destruction {
        return Err("The two passwords must be different.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rules() {
        assert!(validate_pair("short", "short", "x", "x").is_err());
        assert!(validate_pair("aaaaaaaaaa", "aaaaaaaaab", "bbbbbbbbbb", "bbbbbbbbbb").is_err());
        assert!(validate_pair("aaaaaaaaaa", "aaaaaaaaaa", "aaaaaaaaaa", "aaaaaaaaaa").is_err());
        assert!(validate_pair("aaaaaaaaaa", "aaaaaaaaaa", "bbbbbbbbbb", "bbbbbbbbbb").is_ok());
        assert!(strength("Tr0ub4dor&3-horse-battery") >= 4);
        assert_eq!(strength("aaaa"), 0);
    }
}
