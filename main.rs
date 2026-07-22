mod sensors;

use self::sensors::Sensors;
use serde::Deserialize;
use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::thread;
use std::time::Duration;
use std::sync::Mutex;
use std::collections::HashSet;
use lazy_static::lazy_static;

#[derive(Deserialize)]
struct Config {
    request_shutdown_battery_percent: Option<f64>,
    force_shutdown_timeout_secs: Option<f64>,
}

lazy_static! {
    static ref failed: Mutex<HashSet<String>> = Default::default();
}

fn read_battery_string(path_bat: &PathBuf, var_name: &str) -> Option<String> {
    let path = format!("{}/{var_name}", path_bat.display());
    match fs::read_to_string(&path) {
        Err(err) => {
            if !failed.lock().unwrap().contains(&path) {
                eprintln!("Error: read {path}: {err}");
                failed.lock().unwrap().insert(path);
            }
            None
        }
        Ok(string) => Some(string.trim().to_owned()),
    }
}

fn read_battery_f64(path_bat: &PathBuf, var_name: &str) -> Option<f64> {
    let path = format!("{}/{var_name}", path_bat.display());
    match fs::read_to_string(&path) {
        Err(err) => {
            if !failed.lock().unwrap().contains(&path) {
                eprintln!("Error: read {path}: {err}");
                failed.lock().unwrap().insert(path);
            }
            None
        }
        Ok(string) => match f64::from_str(string.trim()) {
            Err(err) => {
                eprintln!("Error: read {path}: {err}");
                None
            }
            Ok(val) => {
                if !val.is_finite() {
                    eprintln!("Error: read {path}: {val} is not finite");
                    None
                } else {
                    Some(val)
                }
            }
        },
    }
}

fn read_battery_maxchargelevel(path: &str) -> Option<f64> {
    // retry 3 times, as there seems to be a strange bug in which some
    // /sys files sometimes disappear, so not adding to the problem by
    // also failing and adding noise to the logs
    for _i in 1..3 {
	let bat_maxchargelevel_from_file = fs::read_to_string(path).unwrap_or("-1.0".to_string());
	let bat_maxchargelevel = i32::from_str(&bat_maxchargelevel_from_file.trim()).unwrap_or(-1);

	if bat_maxchargelevel == 0 {
	    // limit is disabled, returning 100% instead
	    return Some(100.0);
	}
	else if bat_maxchargelevel > 0 {
	    // success, returning supposedly good value
	    return Some(bat_maxchargelevel as f64);
	}
	else {
	    // problem, sleep and retry
	    thread::sleep(Duration::from_millis(333));
	}
    }

    // default
    if !failed.lock().unwrap().contains(path) {
	eprintln!("Error: read '{path}': could not read from file 3 times in a row");
        failed.lock().unwrap().insert(path.to_string());
    }
    None
}

fn write_str(dir_path: &str, var_name: &str, val: Option<&str>) {
    let val = match val {
        Some(val) => val,
        None => return,
    };

    if let Err(err) = fs::create_dir(dir_path) {
        if err.kind() != io::ErrorKind::AlreadyExists {
            eprintln!("Error: mkdir {dir_path}: {err}");
            return;
        }
    }

    // Write to a temporary path first.
    let dot_path = format!("{dir_path}/.{var_name}");
    if let Err(err) = fs::write(&dot_path, format!("{val}\n")) {
        eprintln!("Error: write {dot_path}: {err}");
        return;
    }

    // Then move into place for atomicity.
    let final_path = format!("{dir_path}/{var_name}");
    if let Err(err) = fs::rename(&dot_path, &final_path) {
        eprintln!("Error: rename {dot_path} -> {final_path}: {err}");
    }
}

fn write_f64(dir_path: &str, var_name: &str, val: Option<f64>) {
    if let Some(val) = val {
        write_str(dir_path, var_name, Some(&val.to_string()))
    }
}

fn is_equal_f64_with_margin(val1: f64, val2: f64, margin: f64) -> bool {
    if (val1 - val2).abs() <= margin {
	true
    }
    else {
	false
    }
}

fn main() {
    // Print version info
    let version = env!("CARGO_PKG_VERSION");
    let name = env!("CARGO_PKG_NAME");
    println!("{name} version {version}");

    // Debug?
    #[allow(non_snake_case)]
    let DEBUG = std::env::var("VPOWER_DEBUG")
        .map(|v| matches!(v.to_lowercase().as_str(), "yes" | "y" | "1"))
        .unwrap_or(false)
	|| cfg!(debug_assertions);
    if DEBUG {
        println!("DBG: Debug mode enabled");
    }

    // Read /etc/vpower.toml
    let config_path = "/etc/vpower.toml";
    let mut request_shutdown_battery_percent = 0.49999998;
    let mut force_shutdown_timeout_secs = 10.0;
    match fs::read(config_path) {
        Err(err) => eprintln!("Error: read {config_path}: {err}"),

        Ok(bytes) => match toml::from_slice::<Config>(&bytes) {
            Err(err) => eprintln!("Error: read {config_path}: {err}"),

            Ok(config) => {
                if let Some(value) = config.request_shutdown_battery_percent {
                    request_shutdown_battery_percent = value;
                }
                if let Some(value) = config.force_shutdown_timeout_secs {
                    force_shutdown_timeout_secs = value;
                }
            }
        },
    }
    println!("Info: Config: request_shutdown_battery_percent: {request_shutdown_battery_percent}");
    println!("Info: Config: force_shutdown_timeout_secs: {force_shutdown_timeout_secs}");

    // Mains/AC
    let mut path_ac = PathBuf::from("");
    let power_supply_paths = fs::read_dir("/sys/class/power_supply/").unwrap();
    for ps in power_supply_paths {
	let path_string_test_base = PathBuf::from(ps.unwrap().path());
	let path_string_test = format!("{}/type", path_string_test_base.display());
	let path_test = Path::new(&path_string_test);
	if ! path_test.exists() {
	    continue;
	}
	let path_test_type: String = fs::read_to_string(path_test).expect("Cannot read path");
	if path_test_type.contains("Mains") {
	    path_ac = PathBuf::from(path_string_test_base);
	    println!("Info: Init: Found AC power supply: '{}'", path_ac.display());
	    break;
	}
    }
    if ! path_ac.exists() {
	println!("Warning: Could not find device for AC/Mains, some functionality might be missing or not accurate.");
    }

    // Try to find reasonable BATn to use (stop at the first),
    // otherwise it's a system without battery -- bail-out
    let mut path_bat = PathBuf::from("");
    for i in 0..9 {
	let path_string_test_base = format!("/sys/class/power_supply/BAT{i}");
	let path_string_test = format!("{path_string_test_base}/type");
	let path_bat_test = Path::new(&path_string_test);
	if ! path_bat_test.exists() {
	    continue;
	}

	let path_bat_test_type: String = fs::read_to_string(path_bat_test).expect("Cannot read path");
	if path_bat_test_type.contains("Battery") {
	    path_bat = PathBuf::from(path_string_test_base);
	    println!("Info: Init: Found battery: '{}'", path_bat.display());
	    break;
	}
    }
    if ! path_bat.exists() {
	println!("Info: Init: This system does not use batteries, stopping.");
	return;
    }

    // Some files that the code further below will attempt to read
    // every second (not all devices might provide them, probably
    // better to keep running for partial functionality than stopping
    // completely)
    let bat_values_filenames = vec!["status", "voltage_min_design", "voltage_now"];
    for expected_file in bat_values_filenames.into_iter() {
	let path_expected_file = PathBuf::from(format!("{}/{expected_file}", path_bat.display()));
	if ! path_expected_file.exists() {
	    println!("Warning: Init: Missing expected file: '{}'", path_expected_file.display());
	}
    }
    // for the following files, names vary between charge_full/now
    // (SteamDeck for example) and energy_full/now
    let mut files_named_charge = true;
    let bat_values_filenames_charge = vec!["charge_full", "charge_now"];
    for expected_file in bat_values_filenames_charge.into_iter() {
	let path_expected_file = PathBuf::from(format!("{}/{expected_file}", path_bat.display()));
	if ! path_expected_file.exists() {
	    // assume files are named energy_*
	    files_named_charge = false;
	    let expected_file_subst = expected_file.replace("charge_", "energy_");
	    let path_expected_file_subst = PathBuf::from(format!("{}/{expected_file_subst}", path_bat.display()));
	    if ! path_expected_file_subst.exists() {
		println!("Warning: Init: Missing expected files: '{}' or '{}'", path_expected_file.display(), path_expected_file_subst.display());
	    }
	    else {
		println!("Info: Init: Using '{}' (instead of '{}')", path_expected_file_subst.display(), expected_file);
	    }
	}
    }
    // the following name varies between current_now and power_now
    let mut files_named_current = true;
    let bat_values_filenames_current = vec!["current_now"];
    for expected_file in bat_values_filenames_current.into_iter() {
	let path_expected_file = PathBuf::from(format!("{}/{expected_file}", path_bat.display()));
	if ! path_expected_file.exists() {
	    // assume files are named power_*
	    files_named_current = false;
	    let expected_file_subst = expected_file.replace("current_", "power_");
	    let path_expected_file_subst = PathBuf::from(format!("{}/{expected_file_subst}", path_bat.display()));
	    if ! path_expected_file_subst.exists() {
		println!("Warning: Init: Missing expected files: '{}' or '{}'", path_expected_file.display(), path_expected_file_subst.display());
	    }
	    else {
		println!("Info: Init: Using '{}' (instead of '{}')", path_expected_file_subst.display(), expected_file);
	    }
	}
    }

    // MaxChargeLevel files
    let maxchargelevel_path_std = path_bat.display().to_string() + "/charge_control_end_threshold";
    let maxchargelevel_filenames = vec![
	// SteamDeck, LCD and OLED models
	"/sys/devices/pci0000:00/0000:00:14.3/PNP0C09:00/VLV0100:00/steamdeck-hwmon/hwmon/hwmon3/max_battery_charge_level",
	// generic value supported by e.g. many consumer laptops
	&maxchargelevel_path_std,
    ];
    let mut path_maxchargelevel_file = PathBuf::from("");
    let mut path_chargetypes_file = PathBuf::from("");
    for maxchargelevel_file in maxchargelevel_filenames.into_iter() {
	if PathBuf::from(maxchargelevel_file).exists() {
	    path_maxchargelevel_file = PathBuf::from(maxchargelevel_file);
	    println!("Info: Init: MaxChargeLevel feature: using '{}'", path_maxchargelevel_file.display());
	    break;
	}
    }
    if path_maxchargelevel_file.as_os_str().is_empty() {
	println!("Warning: Init: MaxChargeLevel feature: Cound not find suitable file for direct retrieval");

	// Indirect MaxChargeLevel retrieval.  For systems with charge_types = "Long
	// Life", like Lenovo Legion Go S, will assume 80%.
	if PathBuf::from(format!("{}/charge_types", path_bat.display())).exists() {
	    path_chargetypes_file = PathBuf::from(format!("{}/charge_types", path_bat.display()));
	    println!("Info: Init: MaxChargeLevel feature: using indirect file '{}'", path_chargetypes_file.display());
	}
	else {
	    println!("Warning: Init: MaxChargeLevel feature: Cound not find suitable indirect file (charge_types) either, assuming MaxChargeLevel=100%");
	}
    }

    // Initialize libsensors.
    let sensors = Sensors::new();
    if sensors.pdvl() == None || sensors.pdam() == None {
	println!("Info: Init: Steam Deck sensors: Cannot read Power Delivery values for the AC Adapter (not a Steam Deck)");
    }
    else {
	println!("Info: Init: Steam Deck sensors: Can read Power Delivery values for the AC Adapter");
    }

    // Keep for heuristics.
    let mut prev_ac_status: Option<&str> = None;
    let mut prev_battery_percent: Option<f64> = None;
    let mut prev_battery_percent_hist: [f64; 9] = [-1.0; 9];

    let mut warning_emitted_charger_low_energy: bool = false;
    let mut warning_emitted_ac_status_unknown: bool = false;
    let mut info_emitted_battery_full: bool = false;

    let mut last_bat_maxchargelevel = -999.9;

    let mut loop_counter : u64 = 0;
    let mut attempts_to_read_current_now : u64 = 0;
    let mut attempts_to_read_power_now : u64 = 0;
    let mut failed_to_read_current_now : u64 = 0;
    let mut failed_to_read_power_now : u64 = 0;

    let mut ac_status_last_change : Option<&str> = None;
    let mut ac_status_last_change_at_loop : u64 = 0;

    // Start.
    println!("Info: Running.");

    // Every second:
    loop {
	loop_counter += 1;

	// Get max charge battery level, if set
	let mut bat_maxchargelevel = 100.0;
	if ! path_maxchargelevel_file.as_os_str().is_empty() {
	    bat_maxchargelevel = match read_battery_maxchargelevel(&path_maxchargelevel_file.display().to_string()) {
		None       => -999.9,
		Some(val)  => val
	    };
	}
	else {
	    // Get max charge battery level indirectly (charge_types='Long Life'
	    // is assumed to limit to 80% (Lenovo Legion Go S models), otherwise
	    // assume 100%)
	    if ! path_chargetypes_file.as_os_str().is_empty() {
		if let Some(content) = read_battery_string(&path_bat, "charge_types") {
		    if content.contains("[Long Life]") {
			bat_maxchargelevel = 80.0;
		    }
		}
	    }
	}

	// sanity check, if out of bounds either take from previous
	// value (if looks ok-ish) or otherwise clamp to sane default
	if bat_maxchargelevel < 0.0 || bat_maxchargelevel > 100.0 {
	    if last_bat_maxchargelevel >= 0.0 && last_bat_maxchargelevel <= 100.0 {
		bat_maxchargelevel = last_bat_maxchargelevel;
	    }
	    else {
		bat_maxchargelevel = 100.0;
	    }
	}

	// update value for next iteration
	if bat_maxchargelevel != last_bat_maxchargelevel {
	    last_bat_maxchargelevel = bat_maxchargelevel;

	    // print new detected value, skipping first time (uninitialized)
	    if last_bat_maxchargelevel >= 0.0 {
		println!("Info: New MaxChargeLevel value detected for battery = {}%", last_bat_maxchargelevel);
	    }
	}

        // Read battery variables.
	let (charge_full, charge_now) = if files_named_charge {
	    // SteamDeck (and others)
            ( read_battery_f64(&path_bat, "charge_full"), read_battery_f64(&path_bat, "charge_now") )
	} else {
	    // Units compared to charge_* files are different, but
	    // these are used in values as ratios =now/full or
	    // percentages, so should be fine as long as it's not
	    // mixed or used in other ways
            ( read_battery_f64(&path_bat, "energy_full"), read_battery_f64(&path_bat, "energy_now") )
	};
        let (current_now, power_now_from_file) = if files_named_current {
	    // SteamDeck (and others)
	    attempts_to_read_current_now += 1;
	    let current_now_value = match read_battery_f64(&path_bat, "current_now") {
		Some(v) => v.abs(), // use absolute value
		None => {
		    failed_to_read_current_now += 1;
		    if (failed_to_read_current_now % 60) == 1 {
			eprintln!("Error: read {}/current_now: {} failed out of {} attempts, loop counter={loop_counter} (throttled message)",
				  path_bat.display(),
				  failed_to_read_current_now,
				  attempts_to_read_current_now);
		    }
		    0.0
		}
	    };
	    ( Some(current_now_value), None )
	}
	else {
	    attempts_to_read_power_now += 1;
	    let power_now_value = match read_battery_f64(&path_bat, "power_now") {
		Some(v) => v,
		None => {
		    failed_to_read_power_now += 1;
		    if (failed_to_read_power_now % 60) == 1 {
			eprintln!("Error: read {}/power_now: {} failed out of {} attempts, loop counter={loop_counter} (throttled message)",
				  path_bat.display(),
				  failed_to_read_power_now,
				  attempts_to_read_power_now);
		    }
		    0.0
		}
	    };
	    ( None, Some(power_now_value) )
	};

        let status = read_battery_string(&path_bat, "status");
        let voltage_min_design = read_battery_f64(&path_bat, "voltage_min_design");
        let voltage_now = read_battery_f64(&path_bat, "voltage_now");

        // Derive battery variables.
        let charge_shutdown = charge_full.map(|charge_full| {
            let rsbp = request_shutdown_battery_percent;
            charge_full * (rsbp / 100.0)
        });

        let power_now = match (voltage_now, current_now, power_now_from_file) {
            (Some(voltage_now), Some(current_now), _) => Some(voltage_now * current_now),
            (Some(voltage_now), None, Some(power_now_from_file)) => Some(voltage_now * power_now_from_file),
            (Some(voltage_now), None, None) => Some(voltage_now * 0.0),
            _ => None,
        };

        // Calculate ac_status.
        let mut ac_status = {
            let ac = read_battery_string(&path_ac, "online");
            match ac.as_deref() {
                Some("0") => Some("Disconnected"),
                Some("1") => {
		    // Assume that it's a capable AC adapter for now in the
		    // first iterations, as it is the most likely.  It will be
		    // reclassified as "Connected slow" later, if not providing
		    // enough energy and the battery is discharging.  If it was
		    // already classified as "Connected slow", keep that.
		    if ac_status_last_change == Some("Connected slow") {
			Some("Connected slow")
		    }
		    else {
			Some("Connected")
		    }
		},
                None => {
                    match status.as_deref() {
                        Some("Full" | "Charging" | "Not charging") => if ac_status_last_change == Some("Connected slow") { Some("Connected slow") } else { Some("Connected") },
                        Some("Discharging") => Some("Disconnected"),
                        _ => None,
                    }
                },
		_ => Some("Disconnected"),
            }
        };

	// On first cycle, initialize ac_status-related vars, change from None
	// to the current one, to indicate no change since the start.
	if loop_counter == 1 {
	    prev_ac_status = ac_status;
	    ac_status_last_change = ac_status;
	    ac_status_last_change_at_loop = loop_counter;
	}

	// On AC adapter disconnection, reset var preventing to repeat warnings
	// about adapter/charger providing insufficient energy
	if ! (ac_status == Some("Connected") || ac_status == Some("Connected slow")) {
	    warning_emitted_charger_low_energy = false;
	}

	// Reset warning "ac_status unknown"
	if ac_status != None {
	    warning_emitted_ac_status_unknown = false;
	}

	// Calculate energy input on the SteamDeck
	let pdam = sensors.pdam();
	let pdvl = sensors.pdvl();
	let pd_power = match (pdvl, pdam) {
	    (Some(pdvl), Some(pdam)) => pdvl * pdam, // Watts.
	    _ => 0.0,
	};

        // Calculate battery_percent.
        let battery_percent = match (charge_now, charge_full) {
            (Some(charge_now), Some(charge_full)) => Some(charge_now / charge_full * 100.0),
            _ => None,
        };
	let battery_reached_maxchargelevel : bool = battery_percent > Some(f64::from(bat_maxchargelevel) * 0.90);

	// In case that some are the default values (happens in the first
	// cycles), fill in with most recent value.
	for i in 0..prev_battery_percent_hist.len() {
	    if prev_battery_percent_hist[i] < 0.0 {
		prev_battery_percent_hist[i] = battery_percent.unwrap_or(-1.0);
	    }
	}
	// Calculate average battery charge %
	let prev_battery_percent_hist_avg =
	    prev_battery_percent_hist.iter().sum::<f64>() / prev_battery_percent_hist.len() as f64;

        // Calculate battery_status.
        let battery_status = match (ac_status, status.as_deref()) {
            (_, Some("Full")) => Some("Full"),
            (_, Some("Discharging")) => Some("Discharging"),
	    // Connected to AC/Mains, whether "Max Charge Level" reached
	    // (="Full"), or if "Charging" or "Discharging" even if not declared
	    // when querying the hardware's status
            (Some("Connected") | Some("Connected slow"), Some("Charging") | Some("Not charging")) =>
		if status.as_deref() == Some("Not charging") && battery_reached_maxchargelevel {
		    Some("Full")
		} else {
		    // Check if the battery is actually charging or discharging,
		    // by comparing to previous percentage of charge.  The
		    // SteamDeck sometimes reports 'Charging' (literal content
		    // of /sys/class/power_supply/BAT1/status) even when the
		    // adapter/charger does not provide enough energy and the
		    // battery is actually (slowly) discharging, so it has to be
		    // fixed.
		    if battery_percent == None || prev_battery_percent == None {
			// In the 1st loop we actually don't know
			Some("Unknown")
		    }
		    else if battery_percent > prev_battery_percent
			|| battery_percent.unwrap_or(-1.0) > prev_battery_percent_hist_avg {
			    Some("Charging")
			}
		    else if is_equal_f64_with_margin(battery_percent.unwrap_or(-1.0), prev_battery_percent.unwrap_or(-1.0), 0.005)
			&& is_equal_f64_with_margin(battery_percent.unwrap_or(-1.0), prev_battery_percent_hist_avg, 0.005) {
			    if DEBUG {
				if loop_counter % 10 == 0 {
				    println!("DBG: battery charge stable / not charging: cur={:.3}% avg={:.3?}%, diff={:.3?}%",
					     prev_battery_percent.unwrap_or(-1.0),
					     prev_battery_percent_hist_avg,
					     (battery_percent.unwrap_or(-1.0) - prev_battery_percent_hist_avg));
				}
			    }

			    // In the SteamDecks at least, when charging with
			    // powerful enough chargers it is updating every second,
			    // but when disconnected or using chargers not providing
			    // enough energy and slowly discharging, the
			    // battery-charge-% value is not updated for several
			    // seconds, so for several cycles "battery_percent ==
			    // prev_battery_percent".
			    Some("Not charging")
			}
		    else {
			// Print error/warning only once (until ac_status
			// changes), waiting a few cycles to stabilize
			if (loop_counter - ac_status_last_change_at_loop) == 5 && ! warning_emitted_charger_low_energy {
			    println!("Warning: AC Adapter connected but battery is actually discharging");
			    warning_emitted_charger_low_energy = true;
			}
			Some("Discharging")
		    }
		},
            _ => {
		if ! warning_emitted_ac_status_unknown {
		    println!("Warning: AC Adapter status unknown: ac_status='{:?}', status='{:?}",
			     ac_status, status);
		    warning_emitted_ac_status_unknown = true;
		}

                // Probably "Unknown" or "Not charging". Use heuristics as a fallback.
                let ordering = match (battery_percent, prev_battery_percent) {
                    (Some(lhs), Some(rhs)) => lhs.partial_cmp(&rhs),
                    _ => None,
                };
                match ordering {
                    Some(Ordering::Less) => Some("Discharging"),
                    Some(Ordering::Greater) => Some("Charging"),
                    _ => {
                        if battery_percent.unwrap_or(0.0) >= 89.5 {
                            // Some batteries won't charge when plugged in above ~90%.
                            // We call this "Full".
                            Some("Full")
                        } else {
                            None
                        }
                    }
                }
            }
        };

	// If AC adapter is Connected as was not initially detected as providing
	// insufficient energy ("weak charger"), but nevertheless does not
	// provide sufficient energy and the battery_status is actually
	// Discharging, consider it "slow".  Also the other way around.
	//
	// This has to be evaluated after battery_status is calculated to
	// 'Discharging' (or 'Charging') for the same reasons explained there,
	// and giving some grace period to the connection to settle.
	if ac_status == Some("Connected") && battery_status == Some("Discharging") {
	    ac_status = Some("Connected slow");
	}
	if ac_status == Some("Connected slow") && battery_status == Some("Charging") {
	    ac_status = Some("Connected");
	}

	// Special actions when battery_status is Full
	if battery_status == Some("Full") {

	    // If battery_status is considered Full, and a AC adapter is
	    // connected, consider it always Conneted and not Connected Slow,
	    // because the charge does slow down to a halt at some point -- so,
	    // avoid unnecessarily alarming users or leaving wrong info in logs
	    if matches!(ac_status, Some("Connected" | "Connected slow")) {
		ac_status = Some("Connected");
	    }

	    // Log that we reached Full and stopped charging
	    if power_now.is_some_and(|val| val <= 0.01) {
		if ! info_emitted_battery_full {
		    println!("Info: Battery '{}' at {} and charging stopped",
			     battery_status.unwrap_or("None"),
			     format!("{:.2}%", battery_percent.unwrap_or(-1.0)));
		    info_emitted_battery_full = true;
		}
	    }
	}
	else {
	    // Reset
	    info_emitted_battery_full = false;
	}

	// Register the last change of ac_status (to grant a grace period and
	// calculate later if it's charging or discharging)
	if prev_ac_status != ac_status {
	    ac_status_last_change = prev_ac_status;
	    ac_status_last_change_at_loop = loop_counter;
	}

	// Print info about AC adapter status changes (connection/disconnection)
	//
	// Note: skip first loop (#1, not #0) as some variables related to
	// calculation of charging/discharging might not be accurate (needs the
	// 2nd cycle to be able to compare with the %-charge of 1st cycle, etc).
	if loop_counter > 1 {
	    let mut energy_input_str = "".to_string();
	    if (ac_status == Some("Connected") || ac_status == Some("Connected slow")) && pd_power > 0.0 {
		energy_input_str = format!(" at {}W",
					   // print up to 2 decimal places, but trimming trailing zeros or '.'
					   format!("{:.2}", pd_power).trim_end_matches('0').trim_end_matches('.').to_string());
	    }

	    let mut charge_str = "unknown".to_string();
	    if battery_percent.unwrap_or(-1.0) >= 0.0 {
		charge_str = format!("{:.2}%", battery_percent.unwrap_or(-1.0));
	    }

	    if loop_counter == 2 {
		println!("Info: AC Adapter status at start: '{}'{energy_input_str}, battery {charge_str}, status: '{}'",
			 ac_status.unwrap_or("None"), battery_status.unwrap_or("None"));
	    }
	    else if (loop_counter - ac_status_last_change_at_loop) == 5 && ac_status_last_change != ac_status {
		println!("Info: AC Adapter status changed '{}'->'{}'{energy_input_str}, battery {charge_str}, status: '{}'",
			 ac_status_last_change.unwrap_or("None"), ac_status.unwrap_or("None"),
			 battery_status.unwrap_or("None"));
	    }
	}

        // Calculate secs_until_battery_full.
        let vars = (charge_full, charge_now, voltage_min_design, power_now);
        let secs_until_battery_full = match vars {
            (Some(charge_full), Some(charge_now), Some(voltage_min_design), Some(power_now)) => {
		let charge_maxlevel = charge_full * (bat_maxchargelevel / 100.0);
                let charge_delta = if charge_now < charge_maxlevel { charge_maxlevel - charge_now } else { 0.0 };
                let hours = if charge_delta == 0.0 { 0.0 } else { charge_delta * voltage_min_design / power_now };
                Some(hours * 3600.0)
            }
            _ => None,
        };

        // Calcuate secs_until_shutdown_request.
        let vars = (charge_now, charge_shutdown, voltage_min_design, power_now);
        let secs_until_shutdown_request = match vars {
            (
                Some(charge_now),
                Some(charge_shutdown),
                Some(voltage_min_design),
                Some(power_now),
            ) => {
                if charge_now > charge_shutdown {
                    let charge_delta = charge_now - charge_shutdown;
                    let hours = charge_delta * voltage_min_design / power_now;
                    Some(hours * 3600.0)
                } else {
                    match ac_status {
                        // Avoid shutdown request while connected.
                        Some("Connected") => Some(1.0),
                        _ => Some(0.0),
                    }
                }
            }
            _ => None,
        };

        // Write to /run/vpower/*
        let dir_path = "/run/vpower";
        write_str(dir_path, "ac_status", ac_status);
        write_f64(dir_path, "battery_percent", battery_percent);
        write_str(dir_path, "battery_status", battery_status);

        let val = secs_until_battery_full;
        write_f64(dir_path, "secs_until_battery_full", val);

        let val = secs_until_shutdown_request;
        write_f64(dir_path, "secs_until_shutdown_request", val);

        // Force shutdown after timeout.
        if secs_until_shutdown_request.map_or(false, |x| x == 0.0) {
            println!("Reached {request_shutdown_battery_percent}% battery.");
            println!("Forcing shutdown in {force_shutdown_timeout_secs} seconds.");
            thread::sleep(Duration::from_secs_f64(force_shutdown_timeout_secs));

            println!("Shutting down now.");
            match Command::new("poweroff").status() {
                Err(err) => panic!("poweroff: {err}"),
                Ok(status) => match status.success() {
                    false => panic!("poweroff: {status}"),
                    true => return,
                },
            }
        }

	// Print if battery_percent (as int) changes, if >= 10% every 5%,
	// otherwise every 1%
	let prev_battery_percent_int = prev_battery_percent.unwrap_or(-1.0).round() as i32;
	let cur_battery_percent_int = battery_percent.unwrap_or(-1.0).round() as i32;
	if prev_battery_percent_int >= 0 && cur_battery_percent_int >= 0 {
	    let arrow = if cur_battery_percent_int > prev_battery_percent_int { "(+)" } else { "(-)" };
	    if cur_battery_percent_int > 20
		&& prev_battery_percent_int != cur_battery_percent_int
		&& (cur_battery_percent_int % 10 == 5 || cur_battery_percent_int % 10 == 0)
	    {
		println!("Info: Battery charge: {:2}% {arrow}", cur_battery_percent_int);
	    }
	    else if cur_battery_percent_int <= 20 && prev_battery_percent_int != cur_battery_percent_int {
		println!("Info: Battery charge: {:2}% {arrow}", cur_battery_percent_int);
		// alternative version:
		// println!("Info: Battery charge: {}%->{}%", prev_battery_percent_int, cur_battery_percent_int);
	    }
	}

        // Update prev_*.
        prev_ac_status = ac_status;
        prev_battery_percent = battery_percent;

	// Update history of battery_percent values
        prev_battery_percent_hist.rotate_right(1);
        prev_battery_percent_hist[0] = battery_percent.unwrap_or(-1.0);
	if DEBUG {
	    if loop_counter % 10 == 0 && (battery_status == Some("Discharging") || battery_status == Some("Not charging")) {
		println!("DBG: battery charge: cur={:.3}% avg={:.3?}%, absdiff={:.3?}%",
			 prev_battery_percent.unwrap_or(-1.0),
			 prev_battery_percent_hist_avg,
			 (battery_percent.unwrap_or(-1.0) - prev_battery_percent_hist_avg).abs());
	    }
	}

        // Sleep until next iteration.
        thread::sleep(Duration::from_secs(1));
    }
}
