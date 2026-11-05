# Build NetFlux on GitHub Actions (no local Rust installation)

This project already has a Windows CI workflow, but its release-build job only
compiles the executable. The additional workflow
`.github/workflows/build-windows.yml` creates a downloadable, runnable package.

## What it does

- Runs on a GitHub-hosted `windows-latest` machine.
- Installs stable Rust and compiles with `cargo build --release`.
- Copies `netflux.exe`, `WinDivert.dll`, `WinDivert64.sys`, and both license texts into one folder.
- Uploads that folder as the `NetFlux-Windows-x64` workflow artifact.

The WinDivert runtime files are required beside the executable. They are
included in the artifact; downloading only `netflux.exe` would not be enough.

## How to run it

1. Open the NetFlux repository and use **Actions** to build the current source.
2. Put the redesigned project source into your fork, preserving the folder
   structure and including this workflow file.
3. Commit and push the changes to the fork's default branch.
4. Open your fork on GitHub and select **Actions**.
5. Select **Build Windows executable** in the workflow list.
6. Click **Run workflow** and confirm the default branch.
7. Wait for the run to finish successfully.
8. Open that run and download the `NetFlux-Windows-x64` artifact at the bottom.

Extract the downloaded ZIP. The folder contains all three runtime files plus the NetFlux and WinDivert license texts.
Run `netflux.exe`; Windows will request Administrator permission because
the existing application manifest requires elevation for WinDivert.

## Important

- This workflow does not install anything on your PC and does not use your
  C: drive for the build. Compilation happens on GitHub's Windows runner.
- It does not modify the bandwidth engine or WinDivert source.
- A successful build proves the code compiled and the package was assembled;
  it does not prove live per-process shaping works on your PC. Test Stremio
  with a low limit after downloading.
- The artifact is retained for 14 days. Run the workflow again if it expires.
- This workflow builds from the exact commit in your fork. Make sure the
  redesigned `src/gui.rs` and `src/main.rs` are committed before running it.
