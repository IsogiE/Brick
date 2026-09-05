Brick 0.3.4 CPU and resource review

The strongest match for sustained CPU on Windows is the hidden-window event-loop bug in eframe 0.33.3. When Brick hides its window with `Visible(false)`, Windows may stop delivering redraw events while eframe leaves its loop in `ControlFlow::Poll`. The loop can then consume CPU continuously. This affects startup minimized and closing to the tray. Upstream confirmed and fixed this in https://github.com/emilk/egui/pull/7905. Brick now pins eframe 0.34.3 and retains the Glow renderer.

The screenshot does not establish which window state triggered the user's 6.2% reading. Code inspection identifies a matching upstream defect; measurements on the affected Windows machine are still needed to confirm the result there.

Other desktop defects fixed:

- Once an app update was offered, its expired check deadline stayed in the UI scheduler, forcing a full redraw every 100 ms indefinitely. Deadlines now use the same eligibility conditions as their tasks.
- Failed settings/log reads did not advance their retry time, causing repeated filesystem/JSON work every 100 ms. Failed attempts now wait for the normal interval.
- An unavailable roster service left another expired deadline active. Inactive, unavailable and running checks are no longer scheduled.
- Network waits used animated spinners that requested immediate repaint, regardless of the application's 100 ms timer. Static busy indicators and worker completion notifications let the UI sleep while waiting.
- Background logic now runs separately from painting. Hidden/minimized windows skip roster polling and periodic view reads while addon updates, authentication and app-update checks continue.

Normal scheduling remains: addon checks every five minutes, presence heartbeats every minute, app-update checks every minute, roster refreshes every 30 seconds while that tab is visible. These workers already used real sleeps or scheduled wakeups; the heartbeat was not a continuous CPU loop. Actual downloads, SHA-256 verification and extraction can cause short CPU bursts. The appropriate target is near-zero sustained idle CPU, not a universal CPU ceiling during installation.

Resource and security changes:

- Enforce byte limits while receiving manifests (1 MiB), signatures (1 KiB), addon archives (64 MiB), installers (256 MiB), roster responses (2 MiB), and presence heartbeat/error responses (16 KiB).
- Enforce signed package sizes before and during reception, and remove the extra full-body memory copy. Packages still use a bounded in-memory buffer.
- Bound ZIP extraction to 20,000 entries and 256 MiB total expanded data, reject inconsistent entry sizes, and clean failed staging directories without deleting installed addons before extraction succeeds.
- Make network deadlines explicit: 10-second connection limit, 30-second metadata limit, 180-second package limit. The old blocking client already had a default 30-second overall timeout.
- Preserve Ed25519 manifest verification, SHA-256 package checks, managed-folder restrictions, OAuth PKCE/state, and Windows DPAPI credential protection. This review did not find a signature bypass or demonstrated credential leak.

Separate presence-server findings remain: cold roster-cache requests can fan out into duplicate Discord calls, failed refreshes have no retry backoff, and uncached token verification has no explicit request/concurrency limit. These affect the VPS and were not changed in this desktop CPU patch. The uncommitted guild-hub feature work was discarded at the user's request.

Validation includes regression tests for overdue and disabled timers, hidden windows, worker completion, idle rendering during network waits, oversized/truncated/endless input, ZIP limits and failed staging cleanup. Before publishing, exercise a Windows release build with the window visible and hidden for at least two minutes, then with an offered app update, offline networking, tray restore, and `--startup`. Check both sustained CPU and that scheduled updates still complete. Record the CPU and memory after startup settles; do not compare debug builds or include the initial install/download burst in the idle average.

Completed checks: `cargo fmt --check`, Linux `cargo check` and all 30 unit tests with `-D warnings`, and Windows GNU-target `cargo check --tests` plus a Windows executable build with `-D warnings`. Applied source files were compared byte-for-byte with the tested copy. No app release was published during the review.

Runtime validation limits: a Linux debug startup smoke test using software GL consumed 0.070 CPU seconds over a 15-second sample after initial setup. In an isolated Wine run, both the published and patched Windows builds consumed similar CPU. Diagnostic tracing of the patched build showed six initial app-logic calls followed by a roughly 60-second scheduled wait, rather than continuous Brick logic/render calls. The Wine CPU comparison is inconclusive and is not a substitute for measuring the affected native Windows system. The framework also forces the window visible after its first paint in both reviewed versions; native `--startup` visibility should be checked in that smoke test.
