//! springchick's typeface, Inter, built in so the shell, keyboard and search
//! look the same everywhere. `Inter-Regular.ttf` is Inter 4.1 (SIL OFL 1.1,
//! `Inter-LICENSE.txt`), taken out of nixpkgs' `Inter.ttc` with fonttools:
//! the variable font is over the repo's 500 KB limit, and only Regular is
//! drawn.
#![forbid(unsafe_code)]

pub const INTER: &[u8] = include_bytes!("../Inter-Regular.ttf");

#[cfg(test)]
mod tests {
    #[test]
    fn is_a_truetype_font() {
        assert_eq!(&super::INTER[..4], &[0, 1, 0, 0]);
    }
}
