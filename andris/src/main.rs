/// The banner printed by the binary: its name and version.
fn banner() -> String {
    format!("andris {}", env!("CARGO_PKG_VERSION"))
}

fn main() {
    println!("{}", banner());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_has_name_and_version() {
        let banner = banner();
        let version = banner.strip_prefix("andris ").unwrap();
        assert!(!version.is_empty(), "no version in banner: {banner:?}");
        assert!(
            !version.chars().any(char::is_whitespace),
            "version contains whitespace: {version:?}"
        );
    }
}
