//! Power source awareness.
//!
//! The daemon sleeps until the next event and then dispatches whatever is due,
//! with no notion of where the machine's electricity is coming from. On a
//! laptop that is the difference between a scheduler and a battery drain: a
//! 15-minute interval agent that shells out to a language model runs 96 times
//! a day whether the lid is open on mains power or the machine is in a bag at
//! 12%.
//!
//! This module is the *decision*, and nothing else: [`PowerSource`] describes
//! what was observed, [`PowerPolicy`] what the operator asked for, and
//! [`should_defer`] combines them. All three are pure, so the policy is
//! testable without a battery to unplug.
//!
//! The *observation* is IO and therefore lives in the binary, at
//! `dotagent::power::detect` — this crate stays free of side effects.
//!
//! The default is [`PowerPolicy::Run`]: an installed dotagent behaves exactly
//! as it did before this module existed until someone opts in.

use serde::{Deserialize, Serialize};

/// What to do with a due run while the machine is on battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerPolicy {
    /// Dispatch regardless of power source. The default — unchanged behavior.
    #[default]
    Run,
    /// Hold the run until the machine is back on mains power.
    ///
    /// Nothing accumulates. Both `expected_at` variants resolve to the *current*
    /// window rather than a backlog, so an agent deferred across four hours of
    /// battery runs once when the charger goes in, not sixteen times.
    Defer,
}

/// Where the machine's power is coming from, as far as we can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    /// Mains power (or a desktop, which is the same thing for our purposes).
    Ac,
    /// Running off the battery, with the charge percentage when readable.
    Battery { percent: Option<u8> },
    /// Undetectable: unsupported platform, or the probe failed.
    ///
    /// Treated as [`PowerSource::Ac`] by [`should_defer`]. A machine whose
    /// power source cannot be read must not have its scheduler silently
    /// disabled — failing to run is the worse failure.
    Unknown,
}

/// Power settings from `config.toml`'s `[power]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerConfig {
    /// Policy applied to every schedule that does not override it.
    pub on_battery: PowerPolicy,
    /// Defer everything below this charge, whatever `on_battery` says.
    ///
    /// `0` (the default) disables the check. This is the escape hatch for the
    /// common shape of the problem: agents are welcome to run on battery in
    /// general, just not when there is nearly none left.
    pub min_battery_percent: u8,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            on_battery: PowerPolicy::Run,
            min_battery_percent: 0,
        }
    }
}

/// Whether a due run should be held back.
///
/// Pure: the daemon reads the power source once per tick and passes it in.
///
/// `policy` is the effective policy — a schedule's `on_battery` when it
/// declares one, the `[power]` default otherwise.
pub fn should_defer(source: PowerSource, policy: PowerPolicy, min_battery_percent: u8) -> bool {
    let PowerSource::Battery { percent } = source else {
        // Ac and Unknown both dispatch. See `PowerSource::Unknown`.
        return false;
    };

    if policy == PowerPolicy::Defer {
        return true;
    }

    // A charge we could not read cannot be below the floor. Guessing "low"
    // would disable the scheduler on every machine whose percentage the probe
    // does not understand.
    match percent {
        Some(p) if min_battery_percent > 0 => p < min_battery_percent,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ac_never_defers_whatever_the_policy_says() {
        assert!(!should_defer(PowerSource::Ac, PowerPolicy::Defer, 90));
        assert!(!should_defer(PowerSource::Ac, PowerPolicy::Run, 90));
    }

    /// A machine we cannot read must keep scheduling. Silently disabling every
    /// agent on an unrecognized platform is a worse failure than running.
    #[test]
    fn unknown_source_is_treated_as_ac() {
        assert!(!should_defer(PowerSource::Unknown, PowerPolicy::Defer, 90));
    }

    #[test]
    fn defer_policy_holds_a_run_on_battery_at_any_charge() {
        let full = PowerSource::Battery { percent: Some(100) };
        assert!(should_defer(full, PowerPolicy::Defer, 0));
    }

    #[test]
    fn run_policy_still_defers_below_the_floor() {
        let low = PowerSource::Battery { percent: Some(11) };
        assert!(should_defer(low, PowerPolicy::Run, 20));

        let ok = PowerSource::Battery { percent: Some(21) };
        assert!(!should_defer(ok, PowerPolicy::Run, 20));
    }

    /// Boundary: `min_battery_percent = 20` means "20 is still fine".
    #[test]
    fn the_floor_is_exclusive() {
        let at_floor = PowerSource::Battery { percent: Some(20) };
        assert!(!should_defer(at_floor, PowerPolicy::Run, 20));
    }

    #[test]
    fn a_zero_floor_disables_the_charge_check() {
        let nearly_dead = PowerSource::Battery { percent: Some(1) };
        assert!(!should_defer(nearly_dead, PowerPolicy::Run, 0));
    }

    /// An unreadable charge must not be guessed as low.
    #[test]
    fn an_unknown_charge_never_trips_the_floor() {
        let no_reading = PowerSource::Battery { percent: None };
        assert!(!should_defer(no_reading, PowerPolicy::Run, 90));
        // ...but an explicit `defer` policy does not depend on the charge.
        assert!(should_defer(no_reading, PowerPolicy::Defer, 0));
    }

    #[test]
    fn default_config_changes_nothing() {
        let cfg = PowerConfig::default();
        assert_eq!(cfg.on_battery, PowerPolicy::Run);
        assert_eq!(cfg.min_battery_percent, 0);
        let on_battery = PowerSource::Battery { percent: Some(3) };
        assert!(!should_defer(
            on_battery,
            cfg.on_battery,
            cfg.min_battery_percent
        ));
    }
}
