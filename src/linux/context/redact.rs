//! Takes secrets out of what a page says before it is written down: credentials and token-like
//! values in URLs, and the obvious ones in console messages (`Authorization: Bearer …`,
//! `password=…`, JWTs, provider key prefixes). A filter by shape and by name, not a guarantee:
//! the bundle is still meant to be read only by the person's own agent.

const REDACTED: &str = "REDACTED";
/// Longer inputs are cut before they are looked at.
const MAX_INPUT_CHARS: usize = 4_000;
/// A URL's longest kept form.
const MAX_URL_CHARS: usize = 200;
/// A query value's or fragment's longest kept form.
const MAX_VALUE_CHARS: usize = 60;
/// Path segments of at least this many id-like characters are taken for secrets (a UUID isn't).
const MIN_SECRET_SEGMENT: usize = 32;
/// Values of at least this many token-like characters are taken for secrets.
const MIN_SECRET_VALUE: usize = 20;

const SECRET_PREFIXES: [&str; 8] = [
    "sk-", "ghp_", "gho_", "github_pat_", "xoxb-", "xoxp-", "AKIA", "glpat-",
];

/// Whether a parameter or field name says its value is a credential.
fn sensitive_key(key: &str) -> bool {
    let key = key.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_ascii_lowercase();
    [
        "token",
        "secret",
        "passw",
        "apikey",
        "api_key",
        "api-key",
        "auth",
        "cookie",
        "credential",
        "signature",
        "session",
    ]
    .iter()
    .any(|word| key.contains(word))
        || matches!(key.as_str(), "key" | "sig" | "jwt" | "code" | "pwd" | "otp" | "sid")
        || key.ends_with("key")
}

fn is_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(len, part)| part.len() == *len && part.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A run of token characters with letters and digits in it, long enough not to be a word or a
/// number; JWTs and provider key prefixes always count.
fn looks_like_secret(value: &str) -> bool {
    let value = value.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if value.starts_with("eyJ") && value.matches('.').count() == 2 {
        return true;
    }
    if value.len() >= 16 && SECRET_PREFIXES.iter().any(|prefix| value.starts_with(prefix)) {
        return true;
    }
    value.len() >= MIN_SECRET_VALUE
        && !is_uuid(value)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-+/=.%".contains(&b))
        && value.bytes().any(|b| b.is_ascii_digit())
        && value.bytes().any(|b| b.is_ascii_alphabetic())
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

fn query_pairs(query: &str) -> String {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) if sensitive_key(key) || looks_like_secret(value) => {
                format!("{key}={REDACTED}")
            }
            Some((key, value)) => format!("{key}={}", cut(value, MAX_VALUE_CHARS)),
            None => cut(pair, MAX_VALUE_CHARS),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// A URL without credentials, with secrets taken out of its path, query and fragment, and cut.
/// `data:` URLs lose their payload.
pub fn url(raw: &str) -> String {
    let raw: String = raw.chars().take(MAX_INPUT_CHARS).collect();
    if raw.starts_with("data:") {
        return "data:…".to_owned();
    }
    let Some((scheme, rest)) = raw.split_once("://") else {
        return cut(&raw, MAX_URL_CHARS);
    };
    let (authority, after) = match rest.find(['/', '?', '#']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let (path_and_query, fragment) = match after.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (after, None),
    };
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };
    let path = path
        .split('/')
        .map(|segment| {
            let id_like = segment.len() >= MIN_SECRET_SEGMENT
                && !is_uuid(segment)
                && segment.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                && segment.bytes().any(|b| b.is_ascii_digit());
            if id_like { REDACTED } else { segment }
        })
        .collect::<Vec<_>>()
        .join("/");
    let mut out = format!("{scheme}://{host}{path}");
    if let Some(query) = query {
        out.push('?');
        out.push_str(&query_pairs(query));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        // `#access_token=…` carries credentials; a route (`#/home`) doesn't.
        if fragment.contains('=') {
            out.push_str(&query_pairs(fragment));
        } else {
            out.push_str(&cut(fragment, MAX_VALUE_CHARS));
        }
    }
    cut(&out, MAX_URL_CHARS)
}

/// One line of a message with the obvious secrets taken out, cut to `max` characters.
pub fn text(raw: &str, max: usize) -> String {
    let raw: String = raw.chars().take(MAX_INPUT_CHARS).collect();
    let mut out: Vec<String> = Vec::new();
    // Tokens still to hide after a name that said a value follows (`Authorization:`).
    let mut hide = 0;
    for token in raw.split_whitespace() {
        let plain = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if hide > 0 {
            if plain.eq_ignore_ascii_case("bearer") || plain.eq_ignore_ascii_case("basic") {
                out.push(token.to_owned());
            } else {
                out.push(REDACTED.to_owned());
                hide -= 1;
            }
            continue;
        }
        if token.contains("://") {
            out.push(url(token));
            continue;
        }
        if let Some(at) = token.find([':', '=']) {
            let (name, value) = (&token[..at], &token[at + 1..]);
            if sensitive_key(name) {
                if value.is_empty() {
                    hide = 1;
                    out.push(token.to_owned());
                } else {
                    out.push(format!("{}{REDACTED}", &token[..=at]));
                }
                continue;
            }
        }
        if plain.eq_ignore_ascii_case("bearer") {
            hide = 1;
            out.push(token.to_owned());
        } else if looks_like_secret(token) {
            out.push(REDACTED.to_owned());
        } else {
            out.push(token.to_owned());
        }
    }
    cut(&out.join(" "), max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_credentials_and_secret_query_values() {
        assert_eq!(
            url("https://user:pw@api.example.com/v1/items?id=42&access_token=abc123&q=cats"),
            "https://api.example.com/v1/items?id=42&access_token=REDACTED&q=cats"
        );
        // A long opaque value under an innocent name.
        assert_eq!(
            url("http://localhost:3000/cb?next=/home&x=9f8e7d6c5b4a39281706f5e4d3c2b1a0"),
            "http://localhost:3000/cb?next=/home&x=REDACTED"
        );
        // OAuth implicit flow puts the credentials in the fragment; a route stays.
        assert_eq!(
            url("https://app.test/cb#access_token=abc&state=1"),
            "https://app.test/cb#access_token=REDACTED&state=1"
        );
        assert_eq!(url("http://localhost:5173/#/settings"), "http://localhost:5173/#/settings");
    }

    #[test]
    fn urls_keep_ids_and_hide_key_like_path_segments() {
        assert_eq!(
            url("http://localhost:3000/api/users/123e4567-e89b-12d3-a456-426614174000"),
            "http://localhost:3000/api/users/123e4567-e89b-12d3-a456-426614174000"
        );
        assert_eq!(
            url("https://api.test/hooks/a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6/run"),
            "https://api.test/hooks/REDACTED/run"
        );
        assert_eq!(url("data:image/png;base64,AAAA"), "data:…");
        assert_eq!(url("about:blank"), "about:blank");
        let long = format!("http://a.test/{}", "p/".repeat(300));
        assert_eq!(url(&long).chars().count(), MAX_URL_CHARS);
    }

    #[test]
    fn messages_lose_the_obvious_secrets() {
        assert_eq!(
            text("Request failed. Authorization: Bearer abc.def.ghi for /api", 200),
            "Request failed. Authorization: Bearer REDACTED for /api"
        );
        assert_eq!(
            text("login failed password=hunter2 user=kit", 200),
            "login failed password=REDACTED user=kit"
        );
        assert_eq!(
            text("token: abcdef then eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig", 200),
            "token: REDACTED then REDACTED"
        );
        assert_eq!(
            text("key sk-abcdefghijklmnop1234 and https://a.test/x?api_key=zz1", 200),
            "key REDACTED and https://a.test/x?api_key=REDACTED"
        );
    }

    #[test]
    fn ordinary_messages_are_left_alone_and_cut_to_one_line() {
        assert_eq!(
            text("TypeError: Cannot read properties of undefined (reading 'map')\n    at f", 200),
            "TypeError: Cannot read properties of undefined (reading 'map') at f"
        );
        assert_eq!(text("Unexpected token < in JSON at position 0", 200), "Unexpected token < in JSON at position 0");
        assert_eq!(text("abcdefghij", 5), "abcd…");
    }
}
