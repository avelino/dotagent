//! Reading the machine's power source.
//!
//! The IO half of `dotagent_core::power`, which holds the policy types and the
//! (pure) decision. It lives in the binary rather than in `dotagent-core`
//! because it shells out on macOS and walks sysfs on Linux, and core is
//! deliberately side-effect free.
//!
//! Everything here is best-effort. A probe that fails returns
//! [`PowerSource::Unknown`], which `should_defer` treats as mains power: a
//! machine whose battery we cannot read must keep running its agents.

use dotagent_core::power::{should_defer, PowerConfig, PowerPolicy, PowerSource};
use dotagent_core::Schedule;

/// The power reading plus the policy to judge it against, resolved once per
/// tick and carried down to each dispatch decision.
///
/// Resolved once because the probe is a subprocess on macOS: a per-schedule
/// probe would spawn `pmset` seventeen times to answer the same question.
#[derive(Debug, Clone, Copy)]
pub struct PowerGate {
    source: PowerSource,
    on_battery: PowerPolicy,
    min_battery_percent: u8,
}

impl PowerGate {
    /// Probe the machine and pair the reading with the configured policy.
    ///
    /// `schedules` must be every schedule that could be dispatched this tick.
    /// The probe is skipped only when *nothing* — neither `[power]` nor any
    /// schedule's own `on_battery` — could defer a run, which is the default
    /// case and is why an untouched install never spawns `pmset` at all.
    ///
    /// Consulting the manifests here is load-bearing, not an optimization
    /// detail: a gate built from the global config alone reports
    /// [`PowerSource::Unknown`] under the default `[power]`, and
    /// `should_defer` treats `Unknown` as mains — so every per-schedule
    /// `on_battery = "defer"` would be silently ignored.
    pub fn detect<'a>(
        config: &PowerConfig,
        schedules: impl IntoIterator<Item = &'a Schedule>,
    ) -> Self {
        let source = if Self::would_probe(config, schedules) {
            detect()
        } else {
            PowerSource::Unknown
        };
        Self::new(source, config)
    }

    /// Whether anything in play could defer a run, and therefore whether the
    /// power source is worth reading.
    ///
    /// Split out of [`PowerGate::detect`] so it can be asserted on a host with
    /// no battery: the *decision to probe* is the part that regressed, and it
    /// is testable in a way the reading itself is not.
    fn would_probe<'a>(
        config: &PowerConfig,
        schedules: impl IntoIterator<Item = &'a Schedule>,
    ) -> bool {
        config.on_battery != PowerPolicy::Run
            || config.min_battery_percent > 0
            || schedules.into_iter().any(|s| {
                s.overrides()
                    .on_battery
                    .is_some_and(|p| p != PowerPolicy::Run)
            })
    }

    /// Pair an already-known reading with a policy. Lets the wiring be tested
    /// without a battery to unplug.
    pub fn new(source: PowerSource, config: &PowerConfig) -> Self {
        Self {
            source,
            on_battery: config.on_battery,
            min_battery_percent: config.min_battery_percent,
        }
    }

    /// Whether this schedule's effective policy holds a due run back.
    ///
    /// A schedule's own `on_battery` wins over the `[power]` default; the
    /// charge floor is global and applies either way.
    pub fn defers(&self, sched: &Schedule) -> bool {
        let policy = sched.overrides().on_battery.unwrap_or(self.on_battery);
        should_defer(self.source, policy, self.min_battery_percent)
    }

    /// Charge at the time of the reading, for the log line explaining a held
    /// run. `None` on mains power or when the probe could not read it.
    pub fn battery_percent(&self) -> Option<u8> {
        match self.source {
            PowerSource::Battery { percent } => percent,
            _ => None,
        }
    }
}

/// Read the current power source. Never panics; returns
/// [`PowerSource::Unknown`] when the platform is unsupported or the probe
/// fails.
pub fn detect() -> PowerSource {
    #[cfg(target_os = "macos")]
    {
        detect_macos()
    }
    #[cfg(target_os = "linux")]
    {
        detect_linux()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        PowerSource::Unknown
    }
}

/// `pmset -g batt` rather than IOKit: it avoids a `core-foundation` dependency
/// and an `unsafe` FFI block for one boolean read a few times an hour. The
/// daemon reads this once per tick, so the subprocess cost is bounded by the
/// tick rate, not by the number of agents.
///
/// Not routed through `dotagent-supervisor` — it is an ad-hoc helper on the
/// same footing as the `osascript` call in the desktop notifier, not an
/// orchestrated agent or plugin.
#[cfg(target_os = "macos")]
fn detect_macos() -> PowerSource {
    let Ok(out) = std::process::Command::new("/usr/bin/pmset")
        .args(["-g", "batt"])
        .output()
    else {
        return PowerSource::Unknown;
    };
    if !out.status.success() {
        return PowerSource::Unknown;
    }
    parse_pmset(&String::from_utf8_lossy(&out.stdout))
}

/// Parses the two lines `pmset -g batt` prints (the gap before `87%` is a
/// literal tab):
///
/// ```text
/// Now drawing from 'Battery Power'
///  -InternalBattery-0 (id=12345)    87%; discharging; 4:41 remaining present: true
/// ```
#[cfg(target_os = "macos")]
fn parse_pmset(stdout: &str) -> PowerSource {
    let drawing_from_battery = stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("Now drawing from"))
        .map(|rest| rest.contains("Battery"));

    match drawing_from_battery {
        Some(false) => PowerSource::Ac,
        // No "Now drawing from" line at all — nothing we can interpret.
        None => PowerSource::Unknown,
        Some(true) => PowerSource::Battery {
            percent: parse_percent(stdout),
        },
    }
}

/// Pulls `87` out of `...(id=12345)\t87%; discharging...`.
#[cfg(target_os = "macos")]
fn parse_percent(stdout: &str) -> Option<u8> {
    stdout.lines().find_map(|line| {
        let (before, _) = line.split_once('%')?;
        // The charge is the *last* run of digits before the `%`, which is what
        // keeps `id=12345` earlier on the line from being read as the charge.
        before
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    })
}

/// sysfs: a supply of `type == "Mains"` with `online == 1` means the charger
/// is in. The percentage comes from whichever `Battery` supply reports one.
#[cfg(target_os = "linux")]
fn detect_linux() -> PowerSource {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return PowerSource::Unknown;
    };

    let mut saw_mains = false;
    let mut on_mains = false;
    let mut percent = None;

    for entry in entries.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
        match kind.trim() {
            "Mains" => {
                saw_mains = true;
                if std::fs::read_to_string(path.join("online")).is_ok_and(|s| s.trim() == "1") {
                    on_mains = true;
                }
            }
            // `scope = Device` is a peripheral's battery — a wireless mouse,
            // keyboard, or headset also reports `type = Battery` with a
            // readable `capacity`, and letting one through would judge
            // `min_battery_percent` against the charge of the mouse.
            "Battery"
                if percent.is_none()
                    && !std::fs::read_to_string(path.join("scope"))
                        .is_ok_and(|s| s.trim() == "Device") =>
            {
                percent = std::fs::read_to_string(path.join("capacity"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u8>().ok());
            }
            _ => {}
        }
    }

    match (saw_mains, on_mains) {
        // No mains supply at all: not a laptop we understand. Don't guess.
        (false, _) => PowerSource::Unknown,
        (true, true) => PowerSource::Ac,
        (true, false) => PowerSource::Battery { percent },
    }
}

#[cfg(test)]
mod gate {
    use super::*;
    use dotagent_core::manifest::ScheduleOverrides;

    fn interval(on_battery: Option<PowerPolicy>) -> Schedule {
        Schedule::Interval {
            id: "every-15min".into(),
            interval_minutes: 15,
            args: Vec::new(),
            overrides: ScheduleOverrides {
                on_battery,
                ..Default::default()
            },
        }
    }

    fn cfg(on_battery: PowerPolicy, min_battery_percent: u8) -> PowerConfig {
        PowerConfig {
            on_battery,
            min_battery_percent,
        }
    }

    const ON_BATTERY: PowerSource = PowerSource::Battery { percent: Some(80) };

    #[test]
    fn a_schedule_override_beats_the_global_default() {
        let gate = PowerGate::new(ON_BATTERY, &cfg(PowerPolicy::Run, 0));
        assert!(
            gate.defers(&interval(Some(PowerPolicy::Defer))),
            "schedule opted out of running on battery"
        );
    }

    /// ...and in the other direction: one expensive schedule set to `defer`
    /// globally must not stop a schedule that explicitly asks to run.
    #[test]
    fn a_schedule_can_opt_back_in_when_the_global_default_defers() {
        let gate = PowerGate::new(ON_BATTERY, &cfg(PowerPolicy::Defer, 0));
        assert!(!gate.defers(&interval(Some(PowerPolicy::Run))));
    }

    #[test]
    fn a_schedule_without_an_override_inherits_the_global_default() {
        let deferring = PowerGate::new(ON_BATTERY, &cfg(PowerPolicy::Defer, 0));
        assert!(deferring.defers(&interval(None)));

        let running = PowerGate::new(ON_BATTERY, &cfg(PowerPolicy::Run, 0));
        assert!(!running.defers(&interval(None)));
    }

    /// The charge floor is global on purpose: "the battery is nearly empty" is
    /// a fact about the machine, not about one schedule's appetite.
    #[test]
    fn the_charge_floor_applies_even_to_a_schedule_that_opted_into_running() {
        let nearly_dead = PowerSource::Battery { percent: Some(4) };
        let gate = PowerGate::new(nearly_dead, &cfg(PowerPolicy::Run, 15));
        assert!(gate.defers(&interval(Some(PowerPolicy::Run))));
    }

    /// Regression: the probe used to be gated on `[power]` alone, so under the
    /// default config `detect` returned `Unknown` — which `should_defer` reads
    /// as mains — and every per-schedule `on_battery = "defer"` was silently
    /// ignored. The override tests above all inject a source via `new`, so
    /// they passed green for the whole time this was broken.
    #[test]
    fn a_schedule_override_alone_is_enough_to_consult_the_power_source() {
        let deferring = interval(Some(PowerPolicy::Defer));
        assert!(
            PowerGate::would_probe(&PowerConfig::default(), [&deferring]),
            "a schedule asking to defer must be able to, under default [power]"
        );
    }

    #[test]
    fn nothing_that_could_defer_means_no_probe() {
        let plain = interval(None);
        let opted_in = interval(Some(PowerPolicy::Run));
        assert!(
            !PowerGate::would_probe(&PowerConfig::default(), [&plain, &opted_in]),
            "an untouched install must never spawn the probe"
        );
    }

    #[test]
    fn a_non_default_global_config_probes_on_its_own() {
        let plain = interval(None);
        assert!(PowerGate::would_probe(
            &cfg(PowerPolicy::Defer, 0),
            [&plain]
        ));
        assert!(PowerGate::would_probe(&cfg(PowerPolicy::Run, 20), [&plain]));
    }

    #[test]
    fn on_mains_nothing_defers() {
        let gate = PowerGate::new(PowerSource::Ac, &cfg(PowerPolicy::Defer, 90));
        assert!(!gate.defers(&interval(Some(PowerPolicy::Defer))));
    }

    /// The default config must not change a single dispatch decision.
    #[test]
    fn the_default_config_never_defers() {
        let gate = PowerGate::new(ON_BATTERY, &PowerConfig::default());
        assert!(!gate.defers(&interval(None)));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn reads_ac_power() {
        let out = "Now drawing from 'AC Power'\n \
                   -InternalBattery-0 (id=12345)\t100%; charged; 0:00 remaining present: true\n";
        assert_eq!(parse_pmset(out), PowerSource::Ac);
    }

    #[test]
    fn reads_battery_with_charge() {
        let out = "Now drawing from 'Battery Power'\n \
                   -InternalBattery-0 (id=12345)\t87%; discharging; 4:41 remaining present: true\n";
        assert_eq!(parse_pmset(out), PowerSource::Battery { percent: Some(87) });
    }

    /// `id=` is digits on the same line, before the `%`. Reading the *first*
    /// run of digits would report a 12345% charge.
    #[test]
    fn the_id_field_is_not_mistaken_for_the_charge() {
        let out = "Now drawing from 'Battery Power'\n \
                   -InternalBattery-0 (id=99999)\t5%; discharging; 0:12 remaining present: true\n";
        assert_eq!(parse_pmset(out), PowerSource::Battery { percent: Some(5) });
    }

    #[test]
    fn empty_output_is_unknown() {
        assert_eq!(parse_pmset(""), PowerSource::Unknown);
    }

    /// On battery but the detail line is missing or unparseable: still on
    /// battery. Losing the percentage must not look like being plugged in.
    #[test]
    fn battery_without_a_readable_percent_is_still_battery() {
        let out = "Now drawing from 'Battery Power'\n(no battery detail)\n";
        assert_eq!(parse_pmset(out), PowerSource::Battery { percent: None });
    }

    /// A desktop reports AC with no battery line at all.
    #[test]
    fn a_machine_with_no_battery_reports_ac() {
        assert_eq!(
            parse_pmset("Now drawing from 'AC Power'\n"),
            PowerSource::Ac
        );
    }
}
