# NetFlux licensing and contribution boundaries

NetFlux is a multi-license project. The licenses apply to different authors' work; they do not replace one another.

## Contributors

- Original Throttle v0.1.2 author: [Francesco Gruosso](https://github.com/FrancescoCoding)
- Original NetFlux contributions: [Amel Okicic](https://github.com/Amel-Okicic)

## 1. Original Throttle code — [Francesco Gruosso](https://github.com/FrancescoCoding)

The original code taken from [Throttle v0.1.2](https://github.com/FrancescoCoding/Throttle/releases/tag/v0.1.2) remains under the MIT License. The MIT notice is preserved in [LICENSE-MIT-THROTTLE](LICENSE-MIT-THROTTLE).

The upstream baseline used to compare the repository is tag `v0.1.2`, commit `5ddccc7b784fc5689f4021ef94edaca0642aa911`.

These files are byte-for-byte identical to their upstream v0.1.2 counterparts and remain MIT-licensed:

- `.cargo/config.toml`
- `src/backend/sockets.rs`
- `src/backend/synthetic.rs`

In files changed since v0.1.2, the portions of Francesco's original code that remain in NetFlux continue to be MIT-licensed. The MIT license does not extend to separate original contributions made by Amel.

## 2. Original NetFlux contributions — [Amel Okicic](https://github.com/Amel-Okicic)

Original, independently authored NetFlux contributions by [Amel Okicic](https://github.com/Amel-Okicic) are licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE-NETFLUX-NONCOMMERCIAL.md).

This includes original NetFlux code, UI redesign and additions, original visual assets, and original project documentation/build materials. The following files and directories were added to NetFlux and were not present in the upstream v0.1.2 tree:

- `.github/workflows/build-windows.yml`
- `.github/workflows/release.yml`
- `GITHUB_ACTIONS_BUILD.md`
- `UI_REDESIGN_NOTES.md`
- `assets/**`
- `netflux.exe.manifest`

The license also applies to Amel's independently authored, copyrightable additions and modifications inside the mixed files listed below. It does not relicense the underlying Francesco code in those files.

### Mixed files

These files contain both material derived from Francesco's original Throttle code and later NetFlux changes:

- `.github/workflows/ci.yml`
- `README.md`
- `build.rs`
- `src/backend/engine.rs`
- `src/backend/flows.rs`
- `src/backend/mod.rs`
- `src/backend/rules.rs`
- `src/gui.rs`
- `src/icon.rs`
- `src/main.rs`
- `src/single_instance.rs`
- `src/types.rs`
- `src/update_check.rs`

For these files, upstream-originating portions remain MIT; Amel's original, copyrightable additions are PolyForm Noncommercial. Do not interpret the entire mixed file as exclusively MIT or exclusively PolyForm Noncommercial. The v0.1.2 upstream tree is the comparison baseline for identifying the inherited material.

Mechanical dependency metadata and generated lockfile content are not claimed as original creative work by this notice.

## 3. What the PolyForm Noncommercial terms permit

The PolyForm license permits personal use for study, research, experimentation, private entertainment and hobby projects without an anticipated commercial application. It also permits noncommercial purposes generally and specifically allows use by the noncommercial organizations described in the license.

It does **not** grant permission to use [Amel Okicic](https://github.com/Amel-Okicic)'s covered contributions for commercial purposes. A separate commercial license or written permission from [Amel Okicic](https://github.com/Amel-Okicic) is required for that use.

The original [Francesco Gruosso](https://github.com/FrancescoCoding) portions remain commercially usable under MIT. However, commercial use of a NetFlux build that includes Amel's PolyForm-licensed contributions requires permission covering those contributions. A company may use the original Throttle v0.1.2 code under MIT independently, while respecting the MIT notice.

Anyone redistributing any Amel-licensed contribution must pass on the PolyForm license terms (or the official license URL) and the Required Notice stated in that license.

## 4. Third-party components

WinDivert is a separate third-party component. Its files remain subject to the license included at [vendor/WinDivert-2.2.2-A/LICENSE](vendor/WinDivert-2.2.2-A/LICENSE). Neither the MIT nor the PolyForm license in this repository replaces WinDivert's terms.

## 5. Release packages

The Windows build and release workflows include the project license index, the upstream MIT license, the PolyForm Noncommercial license and the WinDivert license in the packaged distribution.
