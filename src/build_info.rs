/// Informational build identifier for local fork candidates (`Some(n)` shows
/// as "(Build n)"); `None` for a public release, which identifies by version
/// alone. Kept separate from the package version so update comparisons
/// remain semantic.
pub const BUILD_NUMBER: Option<u32> = None;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn identity() -> String {
    match BUILD_NUMBER {
        Some(build) => format!("{VERSION} (Build {build})"),
        None => VERSION.to_string(),
    }
}

/// The build number for the operational log, `release` for a public build.
pub fn build_label() -> String {
    BUILD_NUMBER.map_or_else(|| "release".to_string(), |build| build.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_keeps_build_number_out_of_the_semantic_version() {
        assert_eq!(VERSION, "1.10.0-veredis.2");
        assert_eq!(BUILD_NUMBER, None, "public release");
        assert_eq!(identity(), "1.10.0-veredis.2");
        assert_eq!(build_label(), "release");
    }
}
