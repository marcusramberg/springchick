//! springchick's typeface, Inter, built in so the shell, keyboard and search
//! look the same everywhere. The two files are Inter 4.1 (SIL OFL 1.1,
//! `Inter-LICENSE.txt`), taken out of nixpkgs' `Inter.ttc` with fonttools:
//! the variable font is over the repo's 500 KB limit, and only Regular and
//! SemiBold are drawn.
#![forbid(unsafe_code)]

pub const INTER: &[u8] = include_bytes!("../Inter-Regular.ttf");
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("../Inter-SemiBold.ttf");

#[cfg(test)]
mod tests {
    #[test]
    fn are_truetype_fonts() {
        assert_eq!(&super::INTER[..4], &[0, 1, 0, 0]);
        assert_eq!(&super::INTER_SEMIBOLD[..4], &[0, 1, 0, 0]);
    }
}
