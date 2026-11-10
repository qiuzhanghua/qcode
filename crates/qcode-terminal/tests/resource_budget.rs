//! Resource bounds: the scrollback cap and what a frame costs (ADR-0062, verification 8).
//!
//! Two kinds of assertion live here, and the split matters:
//!
//! - [`history_is_capped_so_a_flooding_child_cannot_grow_memory_forever`] is a
//!   *deterministic invariant* and runs with the normal suite. A pty that writes
//!   without pause must not make the terminal grow without bound — scrollback is the
//!   only thing that could, so the cap is the memory bound. It is checked through the
//!   scroll position rather than by reading RSS, because a byte count is a property of
//!   this machine's allocator and this assertion has to hold everywhere.
//! - the `#[ignore]` baseline measures RSS and per-frame snapshot cost for eight
//!   flooding terminals. Those numbers are machine-dependent, so they print and are
//!   checked only against generous ceilings; `scripts/perf-session.sh` runs them.
//!
//! The frame cost measured is the snapshot, not the paint: `qcode-terminal` has no
//! renderer (ADR-0062 keeps GPUI out of this crate), and the snapshot is the part of a
//! frame that this crate controls. A frame that is slow because the snapshot copies
//! 24 rows is a terminal bug; a frame that is slow because the GPU is busy is not.

#![cfg(unix)]

use std::time::{Duration, Instant};

use qcode_terminal::alacritty_terminal::term::cell::Cell;
use qcode_terminal::{Session, SessionConfig};

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    condition()
}

/// Resident set size in KiB, from `/proc/self/status`.
///
/// `/proc/self/statm` would do too, but `status` names the field, so a reader does not
/// have to remember the column order.
fn resident_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[test]
fn history_is_capped_so_a_flooding_child_cannot_grow_memory_forever() {
    // A child that prints hundreds of thousands of lines. Scrollback is set small so
    // the test is fast and the cap is easy to reason about.
    const SCROLLBACK: usize = 200;

    let mut config =
        SessionConfig::command("/bin/sh", ["-c".to_string(), "yes qcode-cap".to_string()]);
    config.scrollback = SCROLLBACK;
    config.screen_lines = 24;
    let session = Session::spawn(config).expect("spawn");

    // Wait until the flood has clearly produced far more than the cap; scrolling back
    // past the cap would otherwise pass simply because nothing had scrolled yet.
    assert!(
        wait_until(|| session.events().wakeups() > 200, Duration::from_secs(10)),
        "the flood never got going, so the cap was not exercised"
    );
    // A moment more, so the history is certainly full.
    std::thread::sleep(Duration::from_millis(200));

    // Asking for more history than exists must clamp, not grow.
    session.scroll(1_000_000);
    let deepest = session.with_view(|view| view.display_offset());
    session.scroll_to_bottom();
    let back_at_live = session.with_view(|view| view.display_offset());

    assert_eq!(back_at_live, 0, "scroll_to_bottom left the view in history");
    assert!(
        deepest <= SCROLLBACK + 24,
        "history reached {deepest} lines with a cap of {SCROLLBACK}: a flooding child \
         would grow memory without bound"
    );
    // And the cap must actually be reachable, or the bound above is vacuous.
    assert!(
        deepest > SCROLLBACK / 2,
        "history only reached {deepest} lines of a {SCROLLBACK} line cap; the flood did \
         not fill it, so the bound was never tested"
    );

    session.close();
}

#[test]
fn a_capped_history_is_still_consistent_after_scrolling() {
    // The cap must not corrupt what is displayed: after scrolling deep into history and
    // back, the live screen has to be current again — a clamped scroll that left the
    // viewport pinned would show stale rows forever.
    let mut config = SessionConfig::command(
        "/bin/sh",
        ["-c".to_string(), "yes qcode-scroll".to_string()],
    );
    config.scrollback = 100;
    let session = Session::spawn(config).expect("spawn");

    assert!(wait_until(
        || session.events().wakeups() > 100,
        Duration::from_secs(10)
    ));

    session.scroll(50);
    let mid = session.with_view(|view| view.display_offset());
    assert!(mid > 0, "scrolling 50 lines back moved nothing");

    session.scroll_to_bottom();
    assert!(
        wait_until(
            || session.with_view(|view| {
                view.display_offset() == 0 && view.text().contains("qcode-scroll")
            }),
            Duration::from_secs(5)
        ),
        "after returning to the live screen the grid did not show current output"
    );

    session.close();
}

/// Eight flooding terminals: the footprint, its cap, and the snapshot cost.
///
/// `#[ignore]` because the numbers depend on the machine. What is asserted is not a
/// byte count but a *shape*: scrollback is the only thing a flooding child can grow,
/// so the footprint must plateau once every history is full, and stay in the same
/// league as `terminals × scrollback × columns × size_of::<Cell>()`. A wrong absolute
/// budget would either fail on a wide window or pass a leak.
///
/// Measured when this was written (80 columns, `Cell` = 24 bytes):
///
/// ```text
/// scrollback      0 → 3.5 MiB,   1 000 → 5.4 MiB,   10 000 → 22.4 MiB
/// ```
///
/// which is the cap and not a leak: 10 000 × 80 × 24 B is 18.3 MiB of grid, and the
/// growth stops (`late_growth = 0` over six seconds of `yes`).
#[test]
#[ignore = "machine-dependent baseline; run with scripts/perf-session.sh"]
fn eight_flooding_terminals_stay_within_the_resource_budget() {
    const TERMINALS: usize = 8;
    const SCROLLBACK: usize = 10_000;
    const COLUMNS: usize = 80;
    /// A frame at 60 Hz has 16 ms; the snapshot has to be a fraction of that.
    const SNAPSHOT_BUDGET: Duration = Duration::from_millis(4);
    /// The grid is not the only allocation (parser, pty buffers, thread stacks), so the
    /// ceiling is the theoretical grid cost with generous room on top.
    const OVERHEAD_FACTOR: u64 = 2;

    let cell = std::mem::size_of::<Cell>() as u64;
    let grid_kib = (TERMINALS as u64) * (SCROLLBACK as u64) * (COLUMNS as u64) * cell / 1024;

    let rss_before = resident_kib().unwrap_or(0);
    let sessions: Vec<Session> = (0..TERMINALS)
        .map(|_| {
            let mut config =
                SessionConfig::command("/bin/sh", ["-c".to_string(), "yes qcode-perf".to_string()]);
            config.scrollback = SCROLLBACK;
            config.columns = COLUMNS;
            Session::spawn(config).expect("spawn")
        })
        .collect();

    // Let the flood fill every scrollback: that is when the footprint should stop.
    assert!(
        wait_until(
            || sessions
                .iter()
                .all(|session| session.events().wakeups() > 500),
            Duration::from_secs(30)
        ),
        "the floods did not all start"
    );
    std::thread::sleep(Duration::from_millis(500));
    let settled = resident_kib().unwrap_or(0);

    // Snapshot cost: what one frame has to copy out, per terminal.
    let mut worst = Duration::ZERO;
    let mut total = Duration::ZERO;
    for session in &sessions {
        let started = Instant::now();
        session.with_view(|view| view.contents());
        let elapsed = started.elapsed();
        total += elapsed;
        worst = worst.max(elapsed);
    }
    let mean = total / TERMINALS as u32;

    // Now the shape: another stretch of flooding must not move the footprint.
    std::thread::sleep(Duration::from_secs(2));
    let later = resident_kib().unwrap_or(0);
    let late_growth = later.saturating_sub(settled);
    let footprint = later.saturating_sub(rss_before);

    println!("  terminals:        {TERMINALS}");
    println!("  snapshot  mean:   {mean:?}");
    println!("  snapshot  worst:  {worst:?}");
    println!("  rss before:       {rss_before} KiB");
    println!("  rss with full history: {later} KiB (+{footprint})");
    println!(
        "  grid cost floor:  {grid_kib} KiB ({SCROLLBACK} lines x {COLUMNS} cols x {cell} B x {TERMINALS})"
    );
    println!("  growth over 2 s of further flooding: {late_growth} KiB");

    for session in sessions {
        session.close();
    }

    // Ordered from most fundamental to most environmental, so a failure names the
    // real problem: a leak is a bug, an over-size footprint is a mis-set cap, and a
    // slow snapshot may just be a busy machine. (The first version checked the
    // snapshot first, and a mutation that removed the cap reported "frame would drop"
    // — true, but not what had gone wrong.)
    assert!(
        late_growth < 4096,
        "the footprint grew {late_growth} KiB during two more seconds of flooding, so \
         something accumulates per read — scrollback is capped, so this is a leak"
    );
    assert!(
        footprint < grid_kib * OVERHEAD_FACTOR,
        "eight flooding terminals with a full history took {footprint} KiB, more than \
         twice the {grid_kib} KiB the capped grid should cost"
    );
    assert!(
        worst < SNAPSHOT_BUDGET,
        "the slowest snapshot took {worst:?} of a {SNAPSHOT_BUDGET:?} budget; a frame would drop"
    );
}
