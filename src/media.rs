//! LTO media generations: the tape *format* a cartridge is written in.
//!
//! ADR-0010 moves the notion of "generation" off the drive (`[[backends.lto]]`
//! used to carry `media_type`/`nominal_capacity` directly) and onto the
//! cartridge: a drive only declares what it can natively write
//! (`generation`), and the medium's actual generation is detected at
//! `volume init` time (`tape::media_detect`). This module is the one place
//! the generation tables live — density codes, native capacities, and the
//! read/write compatibility matrix — each as literal, hand-checked data, per
//! the ADR's explicit instruction: the LTO compatibility chart stopped being
//! a clean "n-1/n-2" formula at LTO-8 (which reads/writes exactly {LTO-7,
//! LTO-7 Type M, LTO-8}, not "two generations back"), so every table here is
//! spelled out as literal match arms rather than derived from the enum's
//! discriminant order.

use std::fmt;

/// One LTO tape generation, in the order the format was released.
///
/// `Lto7M8` ("LTO-7 Type M") is a distinct *media* format that sits between
/// LTO-7 and LTO-8 in this ordering — it is not a drive generation (no drive
/// natively writes M8 as its own format; only an LTO-8 drive writes it, onto
/// re-purposed LTO-7 media). Its placement here is exactly why the
/// compatibility tables below must be explicit match arms: any arithmetic on
/// the discriminant ("prev" = n-1, "prev-1" = n-2) skips over or miscounts
/// this variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Generation {
    Lto1,
    Lto2,
    Lto3,
    Lto4,
    Lto5,
    Lto6,
    Lto7,
    Lto7M8,
    Lto8,
    Lto9,
    Lto10,
}

impl Generation {
    /// Parse an operator- or config-facing generation string.
    ///
    /// Case-insensitive; accepts the canonical form (`LTO-6`), the bare form
    /// (`LTO6`), the short form (`L6`), and, for LTO-7 Type M specifically,
    /// every spelling in circulation: `LTO-7-M8`, `LTO7M8`, `M8`.
    pub fn parse(s: &str) -> Option<Self> {
        let normalized: String = s
            .trim()
            .to_ascii_uppercase()
            .chars()
            .filter(|c| *c != '-' && *c != '_' && *c != ' ')
            .collect();
        match normalized.as_str() {
            "LTO1" | "L1" => Some(Self::Lto1),
            "LTO2" | "L2" => Some(Self::Lto2),
            "LTO3" | "L3" => Some(Self::Lto3),
            "LTO4" | "L4" => Some(Self::Lto4),
            "LTO5" | "L5" => Some(Self::Lto5),
            "LTO6" | "L6" => Some(Self::Lto6),
            "LTO7" | "L7" => Some(Self::Lto7),
            "LTO7M8" | "M8" | "LTO7TYPEM" => Some(Self::Lto7M8),
            "LTO8" | "L8" => Some(Self::Lto8),
            "LTO9" | "L9" => Some(Self::Lto9),
            "LTO10" | "L10" => Some(Self::Lto10),
            _ => None,
        }
    }

    /// Canonical string form — what `parse` accepts back and what tapectl
    /// writes to config, the DB, and the on-tape ID thunk.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lto1 => "LTO-1",
            Self::Lto2 => "LTO-2",
            Self::Lto3 => "LTO-3",
            Self::Lto4 => "LTO-4",
            Self::Lto5 => "LTO-5",
            Self::Lto6 => "LTO-6",
            Self::Lto7 => "LTO-7",
            Self::Lto7M8 => "LTO-7-M8",
            Self::Lto8 => "LTO-8",
            Self::Lto9 => "LTO-9",
            Self::Lto10 => "LTO-10",
        }
    }

    /// The `st` driver's density code for this generation, as listed in
    /// mt-st's `mt.c`. `Lto10` has none published for the `st` driver at
    /// ADR-0010's writing, hence `None`.
    pub fn density_code(self) -> Option<u8> {
        match self {
            Self::Lto1 => Some(0x40),
            Self::Lto2 => Some(0x42),
            Self::Lto3 => Some(0x44),
            Self::Lto4 => Some(0x46),
            Self::Lto5 => Some(0x58),
            Self::Lto6 => Some(0x5A),
            Self::Lto7 => Some(0x5C),
            Self::Lto7M8 => Some(0x5D),
            Self::Lto8 => Some(0x5E),
            Self::Lto9 => Some(0x60),
            Self::Lto10 => None,
        }
    }

    /// The inverse of [`Self::density_code`].
    ///
    /// `0x40` is accepted as `Lto1` even though it is shared with DLT1 in the
    /// `st` driver's table — ADR-0010 documents this as a known ambiguity
    /// tapectl accepts rather than refuses, since a real LTO-1 cartridge must
    /// still be detectable.
    pub fn from_density_code(code: u8) -> Option<Self> {
        match code {
            0x40 => Some(Self::Lto1),
            0x42 => Some(Self::Lto2),
            0x44 => Some(Self::Lto3),
            0x46 => Some(Self::Lto4),
            0x58 => Some(Self::Lto5),
            0x5A => Some(Self::Lto6),
            0x5C => Some(Self::Lto7),
            0x5D => Some(Self::Lto7M8),
            0x5E => Some(Self::Lto8),
            0x60 => Some(Self::Lto9),
            _ => None,
        }
    }

    /// Marketed (decimal) native capacity in bytes, uncompressed.
    ///
    /// LTO-10 ships in both 30 TB and 40 TB cartridges — a single per-
    /// generation figure cannot express that, so this returns the 30 TB
    /// figure and a 40 TB cartridge is declared explicitly via
    /// `cartridge register --capacity`, which is checked ahead of this table
    /// in every capacity resolution (ADR-0010).
    pub fn native_capacity_bytes(self) -> u64 {
        match self {
            Self::Lto1 => 100_000_000_000,
            Self::Lto2 => 200_000_000_000,
            Self::Lto3 => 400_000_000_000,
            Self::Lto4 => 800_000_000_000,
            Self::Lto5 => 1_500_000_000_000,
            Self::Lto6 => 2_500_000_000_000,
            Self::Lto7 => 6_000_000_000_000,
            Self::Lto7M8 => 9_000_000_000_000,
            Self::Lto8 => 12_000_000_000_000,
            Self::Lto9 => 18_000_000_000_000,
            Self::Lto10 => 30_000_000_000_000,
        }
    }

    /// Can a drive whose native generation is `drive` write media of
    /// generation `media`?
    ///
    /// Explicit match table, per the LTO consortium's published
    /// compatibility chart (lto.org/lto-generation-compatibility):
    /// generations 1-7 write their own generation and the one immediately
    /// prior; LTO-8 writes {LTO-7, LTO-7 Type M, LTO-8} (not "two back" —
    /// LTO-6 is NOT writable by an LTO-8 drive); LTO-9 writes {LTO-8,
    /// LTO-9}; LTO-10 writes {LTO-10} only. `Lto7M8` can never be a *drive*
    /// generation — it names a media format an LTO-8 drive writes onto
    /// LTO-7-class media, not a drive that exists — so `can_write(Lto7M8, _)`
    /// is always `false`.
    pub fn can_write(drive: Self, media: Self) -> bool {
        use Generation::*;
        match drive {
            Lto1 => matches!(media, Lto1),
            Lto2 => matches!(media, Lto2 | Lto1),
            Lto3 => matches!(media, Lto3 | Lto2),
            Lto4 => matches!(media, Lto4 | Lto3),
            Lto5 => matches!(media, Lto5 | Lto4),
            Lto6 => matches!(media, Lto6 | Lto5),
            Lto7 => matches!(media, Lto7 | Lto6),
            Lto7M8 => false,
            Lto8 => matches!(media, Lto8 | Lto7M8 | Lto7),
            Lto9 => matches!(media, Lto9 | Lto8),
            Lto10 => matches!(media, Lto10),
        }
    }

    /// Can a drive whose native generation is `drive` read media of
    /// generation `media`?
    ///
    /// Generations 1-7 read their own, the prior, and two generations back;
    /// LTO-8 reads exactly {LTO-7, LTO-7 Type M, LTO-8}; LTO-9 reads exactly
    /// {LTO-8, LTO-9}; LTO-10 reads {LTO-10} only. `Lto7M8` is never a drive
    /// generation, so `can_read(Lto7M8, _)` is always `false`.
    pub fn can_read(drive: Self, media: Self) -> bool {
        use Generation::*;
        match drive {
            Lto1 => matches!(media, Lto1),
            Lto2 => matches!(media, Lto2 | Lto1),
            Lto3 => matches!(media, Lto3 | Lto2 | Lto1),
            Lto4 => matches!(media, Lto4 | Lto3 | Lto2),
            Lto5 => matches!(media, Lto5 | Lto4 | Lto3),
            Lto6 => matches!(media, Lto6 | Lto5 | Lto4),
            Lto7 => matches!(media, Lto7 | Lto6 | Lto5),
            Lto7M8 => false,
            Lto8 => matches!(media, Lto8 | Lto7M8 | Lto7),
            Lto9 => matches!(media, Lto9 | Lto8),
            Lto10 => matches!(media, Lto10),
        }
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse ----

    #[test]
    fn parse_accepts_canonical_bare_and_short_forms() {
        for (input, expected) in [
            ("LTO-6", Generation::Lto6),
            ("LTO6", Generation::Lto6),
            ("L6", Generation::Lto6),
            ("lto-6", Generation::Lto6),
            ("l6", Generation::Lto6),
        ] {
            assert_eq!(Generation::parse(input), Some(expected), "input {input:?}");
        }
    }

    #[test]
    fn parse_accepts_every_lto7_type_m_spelling() {
        for input in ["LTO-7-M8", "M8", "m8", "LTO7M8", "lto-7-m8"] {
            assert_eq!(
                Generation::parse(input),
                Some(Generation::Lto7M8),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn parse_accepts_lto10() {
        assert_eq!(Generation::parse("LTO-10"), Some(Generation::Lto10));
        assert_eq!(Generation::parse("LTO10"), Some(Generation::Lto10));
        assert_eq!(Generation::parse("L10"), Some(Generation::Lto10));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(Generation::parse(""), None);
        assert_eq!(Generation::parse("DLT1"), None);
        assert_eq!(Generation::parse("LTO-11"), None);
        assert_eq!(Generation::parse("banana"), None);
    }

    #[test]
    fn as_str_round_trips_through_parse() {
        for g in [
            Generation::Lto1,
            Generation::Lto2,
            Generation::Lto3,
            Generation::Lto4,
            Generation::Lto5,
            Generation::Lto6,
            Generation::Lto7,
            Generation::Lto7M8,
            Generation::Lto8,
            Generation::Lto9,
            Generation::Lto10,
        ] {
            assert_eq!(Generation::parse(g.as_str()), Some(g));
        }
    }

    #[test]
    fn display_matches_as_str() {
        assert_eq!(Generation::Lto6.to_string(), "LTO-6");
        assert_eq!(Generation::Lto7M8.to_string(), "LTO-7-M8");
    }

    // ---- density codes: one assertion per row ----

    #[test]
    fn density_code_lto1() {
        assert_eq!(Generation::Lto1.density_code(), Some(0x40));
        assert_eq!(Generation::from_density_code(0x40), Some(Generation::Lto1));
    }
    #[test]
    fn density_code_lto2() {
        assert_eq!(Generation::Lto2.density_code(), Some(0x42));
        assert_eq!(Generation::from_density_code(0x42), Some(Generation::Lto2));
    }
    #[test]
    fn density_code_lto3() {
        assert_eq!(Generation::Lto3.density_code(), Some(0x44));
        assert_eq!(Generation::from_density_code(0x44), Some(Generation::Lto3));
    }
    #[test]
    fn density_code_lto4() {
        assert_eq!(Generation::Lto4.density_code(), Some(0x46));
        assert_eq!(Generation::from_density_code(0x46), Some(Generation::Lto4));
    }
    #[test]
    fn density_code_lto5() {
        assert_eq!(Generation::Lto5.density_code(), Some(0x58));
        assert_eq!(Generation::from_density_code(0x58), Some(Generation::Lto5));
    }
    #[test]
    fn density_code_lto6() {
        assert_eq!(Generation::Lto6.density_code(), Some(0x5A));
        assert_eq!(Generation::from_density_code(0x5A), Some(Generation::Lto6));
    }
    #[test]
    fn density_code_lto7() {
        assert_eq!(Generation::Lto7.density_code(), Some(0x5C));
        assert_eq!(Generation::from_density_code(0x5C), Some(Generation::Lto7));
    }
    #[test]
    fn density_code_lto7_type_m() {
        assert_eq!(Generation::Lto7M8.density_code(), Some(0x5D));
        assert_eq!(
            Generation::from_density_code(0x5D),
            Some(Generation::Lto7M8)
        );
    }
    #[test]
    fn density_code_lto8() {
        assert_eq!(Generation::Lto8.density_code(), Some(0x5E));
        assert_eq!(Generation::from_density_code(0x5E), Some(Generation::Lto8));
    }
    #[test]
    fn density_code_lto9() {
        assert_eq!(Generation::Lto9.density_code(), Some(0x60));
        assert_eq!(Generation::from_density_code(0x60), Some(Generation::Lto9));
    }
    #[test]
    fn density_code_lto10_is_none() {
        assert_eq!(Generation::Lto10.density_code(), None);
    }
    #[test]
    fn from_density_code_rejects_unknown() {
        assert_eq!(Generation::from_density_code(0x00), None);
        assert_eq!(Generation::from_density_code(0xFF), None);
    }

    // ---- native capacities: one assertion per row ----

    #[test]
    fn native_capacity_lto1() {
        assert_eq!(Generation::Lto1.native_capacity_bytes(), 100_000_000_000);
    }
    #[test]
    fn native_capacity_lto2() {
        assert_eq!(Generation::Lto2.native_capacity_bytes(), 200_000_000_000);
    }
    #[test]
    fn native_capacity_lto3() {
        assert_eq!(Generation::Lto3.native_capacity_bytes(), 400_000_000_000);
    }
    #[test]
    fn native_capacity_lto4() {
        assert_eq!(Generation::Lto4.native_capacity_bytes(), 800_000_000_000);
    }
    #[test]
    fn native_capacity_lto5() {
        assert_eq!(Generation::Lto5.native_capacity_bytes(), 1_500_000_000_000);
    }
    #[test]
    fn native_capacity_lto6() {
        assert_eq!(Generation::Lto6.native_capacity_bytes(), 2_500_000_000_000);
    }
    #[test]
    fn native_capacity_lto7() {
        assert_eq!(Generation::Lto7.native_capacity_bytes(), 6_000_000_000_000);
    }
    #[test]
    fn native_capacity_lto7_type_m() {
        assert_eq!(
            Generation::Lto7M8.native_capacity_bytes(),
            9_000_000_000_000
        );
    }
    #[test]
    fn native_capacity_lto8() {
        assert_eq!(Generation::Lto8.native_capacity_bytes(), 12_000_000_000_000);
    }
    #[test]
    fn native_capacity_lto9() {
        assert_eq!(Generation::Lto9.native_capacity_bytes(), 18_000_000_000_000);
    }
    #[test]
    fn native_capacity_lto10() {
        assert_eq!(
            Generation::Lto10.native_capacity_bytes(),
            30_000_000_000_000
        );
    }

    // ---- compatibility matrix: every (drive, media) pair, Lto5..Lto10,
    // both directions, pinned literally (no loop over discriminants: the
    // whole point is that arithmetic on the enum order gets this wrong). ----

    const GENS: [Generation; 6] = [
        Generation::Lto5,
        Generation::Lto6,
        Generation::Lto7,
        Generation::Lto7M8,
        Generation::Lto8,
        Generation::Lto9,
    ];

    fn expected_write(drive: Generation, media: Generation) -> bool {
        use Generation::*;
        matches!(
            (drive, media),
            (Lto5, Lto5)
                | (Lto5, Lto4)
                | (Lto6, Lto6)
                | (Lto6, Lto5)
                | (Lto7, Lto7)
                | (Lto7, Lto6)
                | (Lto8, Lto8)
                | (Lto8, Lto7M8)
                | (Lto8, Lto7)
                | (Lto9, Lto9)
                | (Lto9, Lto8)
                | (Lto10, Lto10)
        )
    }

    fn expected_read(drive: Generation, media: Generation) -> bool {
        use Generation::*;
        matches!(
            (drive, media),
            (Lto5, Lto5)
                | (Lto5, Lto4)
                | (Lto5, Lto3)
                | (Lto6, Lto6)
                | (Lto6, Lto5)
                | (Lto6, Lto4)
                | (Lto7, Lto7)
                | (Lto7, Lto6)
                | (Lto7, Lto5)
                | (Lto8, Lto8)
                | (Lto8, Lto7M8)
                | (Lto8, Lto7)
                | (Lto9, Lto9)
                | (Lto9, Lto8)
                | (Lto10, Lto10)
        )
    }

    #[test]
    fn write_matrix_matches_the_published_compatibility_chart() {
        for &drive in &GENS {
            for &media in &GENS {
                assert_eq!(
                    Generation::can_write(drive, media),
                    expected_write(drive, media),
                    "can_write({drive:?}, {media:?})"
                );
            }
        }
        // Lto10 only ever writes/reads itself; not in GENS (no cross-gen
        // pairs with the others make sense), checked directly instead.
        assert!(Generation::can_write(Generation::Lto10, Generation::Lto10));
        assert!(!Generation::can_write(Generation::Lto10, Generation::Lto9));
    }

    #[test]
    fn read_matrix_matches_the_published_compatibility_chart() {
        for &drive in &GENS {
            for &media in &GENS {
                assert_eq!(
                    Generation::can_read(drive, media),
                    expected_read(drive, media),
                    "can_read({drive:?}, {media:?})"
                );
            }
        }
        assert!(Generation::can_read(Generation::Lto10, Generation::Lto10));
        assert!(!Generation::can_read(Generation::Lto10, Generation::Lto9));
    }

    // ---- named, explicit assertions the spec calls out by name: M8 sits
    // between Lto7 and Lto8 in the enum, so any prev/prev-1 arithmetic lands
    // on it wrong. These pin the exact cases arithmetic would get wrong. ----

    #[test]
    fn lto7_drive_cannot_write_or_read_type_m8_media() {
        assert!(!Generation::can_write(Generation::Lto7, Generation::Lto7M8));
        assert!(!Generation::can_read(Generation::Lto7, Generation::Lto7M8));
    }

    #[test]
    fn lto9_drive_cannot_write_type_m8_media() {
        assert!(!Generation::can_write(Generation::Lto9, Generation::Lto7M8));
    }

    #[test]
    fn lto9_drive_cannot_read_lto7_media() {
        assert!(!Generation::can_read(Generation::Lto9, Generation::Lto7));
    }

    #[test]
    fn lto8_drive_can_write_type_m8_media() {
        assert!(Generation::can_write(Generation::Lto8, Generation::Lto7M8));
    }

    #[test]
    fn lto7_type_m8_is_never_a_writable_drive_generation() {
        assert!(!Generation::can_write(Generation::Lto7M8, Generation::Lto7));
        for &media in &GENS {
            assert!(!Generation::can_write(Generation::Lto7M8, media));
            assert!(!Generation::can_read(Generation::Lto7M8, media));
        }
    }
}
