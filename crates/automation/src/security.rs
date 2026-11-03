//! Authentication for the loopback control transport.

use std::io::{self, Read, Write};
use std::path::Path;
use subtle::ConstantTimeEq;

pub fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn token_matches(expected: &str, supplied: &str) -> bool {
    if expected.len() != 64 || supplied.len() != 64 {
        return false;
    }
    let expected = expected.to_ascii_lowercase();
    let supplied = supplied.to_ascii_lowercase();
    bool::from(expected.as_bytes().ct_eq(supplied.as_bytes()))
}

pub fn read_token(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("control token is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 {
            return Err(io::Error::other("control token file must not be accessible by group or others"));
        }
    }
    let mut token = String::new();
    Read::by_ref(&mut file).take(128).read_to_string(&mut token)?;
    let token = token.trim();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::other("control token must contain 64 hexadecimal characters"));
    }
    Ok(token.to_owned())
}

pub fn write_token(path: &Path, token: &str) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    writeln!(options.open(path)?, "{token}")
}

pub fn client_token() -> io::Result<String> {
    match (std::env::var("SOUNDCRAFT_CONTROL_TOKEN").ok(), std::env::var_os("SOUNDCRAFT_CONTROL_TOKEN_FILE")) {
        (Some(_), Some(_)) => Err(io::Error::other("set only one of SOUNDCRAFT_CONTROL_TOKEN and SOUNDCRAFT_CONTROL_TOKEN_FILE")),
        (Some(token), None) if token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) => Ok(token),
        (Some(_), None) => Err(io::Error::other("control token must contain 64 hexadecimal characters")),
        (None, Some(path)) => read_token(Path::new(&path)),
        (None, None) => Err(io::Error::other("set SOUNDCRAFT_CONTROL_TOKEN or SOUNDCRAFT_CONTROL_TOKEN_FILE")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_distinct_and_match_in_constant_width() {
        let first = generate_token().expect("random token");
        let second = generate_token().expect("random token");
        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
        assert!(token_matches(&first, &first.to_ascii_uppercase()));
        assert!(!token_matches(&first, &second));
        assert!(!token_matches(&first, "short"));
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_private_and_never_overwritten() {
        use std::os::unix::fs::MetadataExt;
        let token = generate_token().expect("random token");
        let path = std::env::temp_dir().join(format!("soundcraft-token-test-{token}"));
        write_token(&path, &token).expect("write token");
        assert_eq!(std::fs::metadata(&path).expect("metadata").mode() & 0o777, 0o600);
        assert_eq!(read_token(&path).expect("read token"), token);
        assert!(write_token(&path, &token).is_err());
        std::fs::remove_file(path).expect("remove token");
    }
}
