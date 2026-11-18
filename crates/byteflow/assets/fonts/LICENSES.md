# Bundled fonts

| File | Font | License |
|---|---|---|
| `PlusJakartaSans-*.ttf` | Plus Jakarta Sans (static Regular, Medium, SemiBold, Bold) from <https://github.com/tokotype/PlusJakartaSans> | SIL Open Font License 1.1 (`OFL.txt`) |
| `NotoSansArabic-VF.ttf` | Noto Sans Arabic v2.013, unhinted slim variable font (`wght` 100–900; the app uses 400/500/600/700), from <https://github.com/notofonts/arabic> | SIL Open Font License 1.1 (`OFL-NotoSansArabic.txt`) |
| `Phosphor.ttf` | Phosphor Icons, regular weight, from <https://github.com/phosphor-icons/core> (as shipped in the `egui-phosphor` crate) | MIT, Copyright (c) 2023 Phosphor Icons |

The fonts are embedded into the binary with `include_bytes!` (see `src/theme.rs`).
