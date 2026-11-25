# Bundled fonts

| File | Font | License |
|---|---|---|
| `Inter-*.ttf` | Inter 4.1 (static Regular, Medium, SemiBold, Bold, from the `extras/ttf` folder of the release) from <https://github.com/rsms/inter> — the UI font. Modified: the cmap entries for the Private Use Area (U+E000–U+F8FF) are removed, so Phosphor's icon codepoints are not shadowed by Inter's own PUA glyphs (glyph outlines unchanged) | SIL Open Font License 1.1 (`OFL-Inter.txt`) |
| `NotoSansArabic-VF.ttf` | Noto Sans Arabic v2.013, unhinted slim variable font (`wght` 100–900; the app uses 400/500/600/700), from <https://github.com/notofonts/arabic> | SIL Open Font License 1.1 (`OFL-NotoSansArabic.txt`) |
| `Phosphor.ttf` | Phosphor Icons, regular weight, from <https://github.com/phosphor-icons/core> (as shipped in the `egui-phosphor` crate) | MIT, Copyright (c) 2023 Phosphor Icons |

The fonts are embedded into the binary with `include_bytes!` (see `src/theme.rs`).
