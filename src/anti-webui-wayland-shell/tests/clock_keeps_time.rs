//! A clock read in a view keeps time after that view runs again.

use anti_webui_core::Ui;
use anti_webui_core::prelude::*;
use anti_webui_wayland_shell::{Every, local_time};
use std::time::Instant;

#[test]
fn a_clock_keeps_its_timer_when_the_view_that_read_it_first_runs_again() {
    // Something else the text reads, such as a locale that arrives after the start.
    let other = signal(0_u32);
    let mut ui = Ui::new(move || {
        div().child(text(move || {
            format!(
                "{} {}",
                other.get(),
                local_time(Every::Minute).format("%H:%M")
            )
        }))
    });
    ui.update();
    let now = Instant::now();
    assert!(
        ui.next_deadline(now).is_some(),
        "the clock waits for the next minute"
    );

    // The text runs again before the first minute has turned: its first run's owner goes.
    other.set(1);
    ui.update();
    assert!(
        ui.next_deadline(now).is_some(),
        "the clock still waits for the next minute"
    );
}
