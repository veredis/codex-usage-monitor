/// Informational build identifier for local fork candidates. This is kept
/// separate from the package version so update comparisons remain semantic.
pub const BUILD_NUMBER: u32 = 4;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn identity() -> String {
    format!("{VERSION} (Build {BUILD_NUMBER})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_keeps_build_number_out_of_the_semantic_version() {
        assert_eq!(identity(), "1.9.1-veredis.7 (Build 4)");
        assert_eq!(VERSION, "1.9.1-veredis.7");
        assert_eq!(BUILD_NUMBER, 4);
    }
}
