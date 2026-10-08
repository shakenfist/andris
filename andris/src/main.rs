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
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 3, "not a semver version: {version}");
        assert!(
            parts.iter().all(|p| p.parse::<u32>().is_ok()),
            "not a semver version: {version}"
        );
    }
}
