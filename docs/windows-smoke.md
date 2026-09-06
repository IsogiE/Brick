# Windows desktop smoke testing

The dedicated VM on LucasPC runs Windows 11 Enterprise Evaluation with KVM, four vCPUs, 8 GiB RAM, and a sparse 64 GiB disk. It does not start automatically with the host. The desktop is available at <http://127.0.0.1:28006>. Windows evaluation licensing lasts 90 days; rebuild or license the VM when it expires.

State lives in `~/.local/share/brick-windows-smoke/`. Only that VM's `shared` and bootstrap directories are exposed to the guest. The browser console and optional RDP port (`127.0.0.1:53389`) bind to localhost. Guest credentials are in the private, mode-600 `windows.env`; do not commit that file or share its contents. The guest has no host repository, Docker socket, or Discord session mounted.

The pinned image is `dockurr/windows@sha256:0cff9eb0e7aee9953e55bc682852ca4fdca233145a58ae1ec94f0b0c01a2ed30`. The bootstrap installs the official Fedora QEMU guest agent and enables UAC before testing. The guest agent is accessed over a private Unix socket, with no network command-execution service. GUI tests run as the logged-in `BrickTest` user with a limited token, not in service session 0.

## Run a candidate

Build candidate packages from the branch or commit to test. Actions artifacts are public now that the source repo is public. Manual Release runs default to `publish=false`; public tags still trigger the normal release path.

```sh
gh workflow run release.yml --repo IsogiE/Brick --ref YOUR_BRANCH -f version=0.3.6 -f publish=false
gh run download RUN_ID --repo IsogiE/Brick -n brick-Windows -D /tmp/brick-windows-candidate
python scripts/windows-vm.py start
python scripts/windows-vm.py run /tmp/brick-windows-candidate/brick_0.3.6_x64-setup.exe --software-gl
```

The command prints the results directory. A pass requires `report.json` to contain `"passed": true`; process launch alone does not count. Keep the installer SHA-256, source commit, report, CSV samples, and screenshots together. Test the final candidate before creating a public release tag.

```sh
python scripts/windows-vm.py status
python scripts/windows-vm.py screenshot
python scripts/windows-vm.py stop
```

Stop the VM when testing is finished to release its RAM and CPU. The control script also supports `ps path/to/check.ps1` for diagnostics through the guest agent. This runs with service privileges; interactive UI work should use a scheduled task with `LogonType Interactive` and `RunLevel Limited`, as the smoke runner does.

## What is checked

- Current-user installation without elevation and the expected LOCALAPPDATA path.
- A live, visible main window after launch.
- Minimize and X both hide Brick into the tray.
- A second launch restores the original window, with only one Brick process.
- `--startup` stays hidden when Start minimized is enabled.
- Sixty seconds each of visible and hidden idle CPU, memory, and window state.
- The window never becomes visible during the hidden measurement interval.

The report uses the same normalization as Task Manager: process CPU divided by the VM's logical processor count. CSVs also include percentages of one CPU core. Check averages and individual samples; a short startup or download burst is different from sustained idle CPU.

Also click the real tray icon and exercise Discord login, WoW folder setup, an actual signed addon update, an already-current recheck, and an app-update prompt before a fully authenticated release sign-off. No host Discord credentials were copied into this VM. The initial automated smoke run covers the signed-out desktop lifecycle; it does not claim authenticated addon installation coverage.

## Virtual graphics

The VirtIO display-only adapter exposes only OpenGL 1.1, while Brick's renderer requires OpenGL 2.0 or newer. The VM uses an **app-local test fixture** built from checksum-verified MSYS2 Mesa packages. `shared/mesa/packages.json` records the package versions and checksums. `--software-gl` copies only the required DLLs next to the installed test executable, selects llvmpipe, and records all DLL hashes in the report. These DLLs are not part of Brick's public packages.

This allows observable UI and idle scheduling tests. Software rendering costs and memory usage differ from a physical GPU, so do not treat VM rendering performance as a physical-PC benchmark. Run without `--software-gl` on a Windows machine with a suitable graphics driver.

The first fresh-VM test of public 0.3.5 caught a missing `VCRUNTIME140.dll`. Windows builds now link the C runtime statically, and the installer verification script checks PE imports so CI machines with an already-installed redistributable cannot mask this problem.

## Evidence from initial setup

The final private 0.3.6 candidate (`24b767e`, build run `33969630732`) passed all 11 automated desktop checks with UAC enabled, plus observed tray-click restoration and X afterward. Over 60-second samples it averaged 0.032% Task Manager CPU while visible and 0.051% while hidden, using four vCPUs and software OpenGL. The installer SHA-256 is `2f44faa7c2933585e476613d3f5a9ed70d94d343ad083cf8357784044a446b7e`. The complete report and CSVs are in `shared/results/20260905-134656/` and manual screenshots alongside them.

The installed CachyOS build passed X, minimize, taskbar removal, tray restore, second-instance restore, and startup-minimized tests. A 60-second hidden sample used 0.017% of one CPU core. The addon-feed workflow run `33969838214` verified that the current source revision skips both packaging and publication.

## Published 0.3.6

Brick [v0.3.6](https://github.com/IsogiE/Brick-Releases/releases/tag/v0.3.6) was published from source commit `33617008e707428a0e1f9679d299b86df50cfc8c` by [Release run 33973247056](https://github.com/IsogiE/Brick/actions/runs/33973247056). Automatic addon updates are always enabled and check once a minute, including while hidden in the tray. "Open at login" controls only startup, and legacy disabled-update settings resume updates while preserving startup preferences.

[CI run 33972495395](https://github.com/IsogiE/Brick/actions/runs/33972495395) passed 34 Linux Rust tests, 35 Windows Rust tests, six addon-feed publication tests, formatting, warning-free checks, and optimized builds. [Private candidate run 33972495091](https://github.com/IsogiE/Brick/actions/runs/33972495091) built the tested packages before the public tag was pushed.

The candidate Windows installer passed all 11 automated desktop checks, plus a real tray-icon click and X after restoration. A configured legacy profile also verified `watcherEnabled` becomes true while `startupEnabled=false` and `startupMinimized=true` are preserved. The CachyOS candidate passed minimize, X, taskbar removal, tray restore, second-instance restore, startup-minimized, and legacy-settings checks. It stayed hidden for 60 seconds with no measurable CPU ticks. The installed app also loaded the existing CachyOS Discord session and roster. Windows desktop smoke coverage used a signed-out test profile.

All five public release assets were downloaded and verified. The signed app feed advertises 0.3.6 and the correct source commit; its Ed25519 signature verifies with the key embedded in public 0.3.4. The public AppImage executable is byte-identical to the CachyOS-tested candidate. The public Windows installer was tested separately after publication and passed all 11 desktop checks again.

Public package SHA-256 values:

- Windows installer: `4155c7fa41393010234c99c02eebf2a4f74a8c12238e3b53c8d3be66df9be14c`
- Linux AppImage: `d7045c9ffd8e40b6c8f80922f406e2909f69ea7ed6e169a22d95c9058720487f`

Durable local evidence is in `~/.local/share/brick-windows-smoke/shared/results/release-0.3.6/`. Candidate Windows CSVs are in `shared/results/20260905-144559/`; public-installer CSVs are in `shared/results/20260905-150139/`. The local CachyOS launcher contains the verified public AppImage.

## Published 0.3.7

Brick [v0.3.7](https://github.com/IsogiE/Brick-Releases/releases/tag/v0.3.7) was published from source commit `b9bdc3466399f569a19d846f4706f34a13507fdf` by [Release run 33978677746](https://github.com/IsogiE/Brick/actions/runs/33978677746). It adds cleanup of recognized completed installers in the system temporary `Brick/updates` cache. Running-version and older downloads are removed, including legacy MSI files; newer pending downloads and locked files are preserved, with locked files retried on later checks. The cleanup does not scan WoW's `Interface` directory. Addon installation code is unchanged from 0.3.6.

[CI run 33978237761](https://github.com/IsogiE/Brick/actions/runs/33978237761) passed 38 Linux Rust tests, 39 Windows Rust tests, six addon-feed publication tests, formatting, warning-free checks, and optimized builds. The Windows tests include retaining a file while Windows holds it open and removing it after the handle closes. [Private candidate run 33978237635](https://github.com/IsogiE/Brick/actions/runs/33978237635) built the packages tested before the public tag was pushed.

The candidate and public Windows installers each passed all 11 automated desktop checks with the VM-only software OpenGL fixture. An initial public-installer visibility assertion failed despite a visible Brick window. The harness previously accepted any titled process window, including hidden IME helpers; it now selects the window titled `Brick`. Window tracing showed the actual Brick window visible after 0.345 seconds. Candidate and public Windows binaries differ only in PE/debug timestamps and the PDB debug GUID; all other bytes match. Packaged-app cleanup checks on Windows and CachyOS removed old/current installer fixtures while preserving newer pending downloads, unrelated files, nested files, and a test WoW Interface file. The CachyOS candidate also passed minimize, X, taskbar removal, tray restoration, second-instance restoration, and startup-minimized checks; it stayed hidden for 60 seconds at 0.017% of one CPU core. These tests used isolated/signed-out profiles; authenticated addon installation was not retested for this cache-only change.

All five public assets were downloaded and verified. The signed app feed advertises 0.3.7 and the correct source commit, and its Ed25519 signature verifies with the key embedded in public 0.3.4. The public AppImage executable is byte-identical to the tested candidate.

Public package SHA-256 values:

- Windows installer: `946000e024ff3d5dce3748dbb6a6c5c6d98861a0ab70e851c63d52399add47e4`
- Linux AppImage: `582ed679d1019896c721984906c74f2d0a41d9d5f71b01b57af32d010f296140`

Durable local evidence is in `~/.local/share/brick-windows-smoke/shared/results/release-0.3.7/`. Original Windows candidate results are in `shared/results/20260905-163823/`; public-installer results are in `shared/results/20260905-165557/`.

Sources: [Microsoft evaluation](https://www.microsoft.com/en-us/evalcenter/download-windows-11-enterprise), [VM wrapper](https://github.com/dockur/windows), [QEMU monitor](https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html), [MSYS2 Mesa](https://packages.msys2.org/packages/mingw-w64-ucrt-x86_64-mesa), [Rust C-runtime linkage](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes).
