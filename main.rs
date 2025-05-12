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
                eprintln!("read {path}: {err}");
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
                eprintln!("read {path}: {err}");
                failed.lock().unwrap().insert(path);
            }
            None
        }
        Ok(string) => match f64::from_str(string.trim()) {
            Err(err) => {
                eprintln!("read {path}: {err}");
                None
            }
            Ok(val) => {
                if !val.is_finite() {
                    eprintln!("read {path}: {val} is not finite");
                    None
                } else {
                    Some(val)
                }
            }
        },
    }
}

fn write_str(dir_path: &str, var_name: &str, val: Option<&str>) {
    let val = match val {
        Some(val) => val,
        None => return,
    };

    if let Err(err) = fs::create_dir(dir_path) {
        if err.kind() != io::ErrorKind::AlreadyExists {
            eprintln!("mkdir {dir_path}: {err}");
            return;
        }
    }

    // Write to a temporary path first.
    let dot_path = format!("{dir_path}/.{var_name}");
    if let Err(err) = fs::write(&dot_path, format!("{val}\n")) {
        eprintln!("write {dot_path}: {err}");
        return;
    }

    // Then move into place for atomicity.
    let final_path = format!("{dir_path}/{var_name}");
    if let Err(err) = fs::rename(&dot_path, &final_path) {
        eprintln!("rename {dot_path} -> {final_path}: {err}");
    }
}

fn write_f64(dir_path: &str, var_name: &str, val: Option<f64>) {
    if let Some(val) = val {
        write_str(dir_path, var_name, Some(&val.to_string()))
    }
}

fn main() {
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
	    println!("Found AC power supply: '{}'", path_ac.display());
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
	    println!("Found battery: {}", path_bat.display());
	    break;
	}
    }
    if ! path_bat.exists() {
	println!("This system does not use batteries, stopping.");
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
	    println!("Warning: missing expected file: {}", path_expected_file.display());
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
		println!("Warning: missing expected files: {} or {}", path_expected_file.display(), path_expected_file_subst.display());
	    }
	    else {
		println!("Info: using {} (instead of '{}')", path_expected_file_subst.display(), expected_file);
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
		println!("Warning: missing expected files: {} or {}", path_expected_file.display(), path_expected_file_subst.display());
	    }
	    else {
		println!("Info: using {} (instead of '{}')", path_expected_file_subst.display(), expected_file);
	    }
	}
    }

    // Read /etc/vpower.toml
    let config_path = "/etc/vpower.toml";
    let mut request_shutdown_battery_percent = 0.49999998;
    let mut force_shutdown_timeout_secs = 10.0;

    match fs::read(config_path) {
        Err(err) => eprintln!("read {config_path}: {err}"),

        Ok(bytes) => match toml::from_slice::<Config>(&bytes) {
            Err(err) => eprintln!("read {config_path}: {err}"),

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

    println!("request_shutdown_battery_percent: {request_shutdown_battery_percent}");
    println!("force_shutdown_timeout_secs: {force_shutdown_timeout_secs}");

    // Initialize libsensors.
    let sensors = Sensors::new();

    // Keep for heuristics.
    let mut prev_ac_status: Option<&str> = None;
    let mut prev_battery_percent: Option<f64> = None;

    // Start.
    println!("Running.");

    // Every second:
    loop {
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
	    ( read_battery_f64(&path_bat, "current_now"), None )
	}
	else {
	    ( None, read_battery_f64(&path_bat, "power_now") )
	};
        let pdam = sensors.pdam();
        let pdcs = sensors.pdcs();
        let pdvl = sensors.pdvl();
        let status = read_battery_string(&path_bat, "status");
        let voltage_min_design = read_battery_f64(&path_bat, "voltage_min_design");
        let voltage_now = read_battery_f64(&path_bat, "voltage_now");

        // Derive battery variables.
        let charge_shutdown = charge_full.map(|charge_full| {
            let rsbp = request_shutdown_battery_percent;
            charge_full * (rsbp / 100.0)
        });

        let power_now = match (voltage_now, current_now) {
            (Some(voltage_now), Some(current_now)) => Some(voltage_now * current_now),
            (Some(voltage_now), None) => Some(power_now_from_file.expect("Error: Missing necessary data: power_now_from_file") * voltage_now),
            _ => None,
        };

        // Calculate ac_status.
        let ac_status = if let Some(pdcs) = pdcs {
            let connected = (pdcs & (1 << 0)) != 0;
            let sink = (pdcs & (1 << 4)) == 0;
            if connected && sink {
                let was_disconnected = prev_ac_status == Some("Disconnected");
                let pd_power = match (pdvl, pdam) {
                    (Some(pdvl), Some(pdam)) => pdvl * pdam, // Watts.
                    _ => 0.0,
                };

                // Basically all power supplies get reported as low power for ~0.5 seconds
                // after connecting, so ignore it on the first iteration after connecting.
                if !was_disconnected && pd_power > 0.0 && pd_power < 30.0 {
                    Some("Connected slow")
                } else {
                    Some("Connected")
                }
            } else {
                Some("Disconnected")
            }
        } else {
            let ac = read_battery_string(&path_ac, "online");
            match ac.as_deref() {
                Some("0") => Some("Disconnected"),
                Some("1") => Some("Connected"),
                None => {
                    match status.as_deref() {
                        Some("Full" | "Charging") => Some("Connected"),
                        Some("Discharging") => Some("Disconnected"),
                        _ => None,
                    }
                },
             _ => Some("Disconnected"),
            }
        };

        // Calculate battery_percent.
        let battery_percent = match (charge_now, charge_full) {
            (Some(charge_now), Some(charge_full)) => Some(charge_now / charge_full * 100.0),
            _ => None,
        };

        // Calculate battery_status.
        let battery_status = match (ac_status, status.as_deref()) {
            (_, Some("Full")) => Some("Full"),
            (_, Some("Discharging")) => Some("Discharging"),
            (Some("Connected"), Some("Charging")) => Some("Charging"),
            _ => {
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

        // Calculate secs_until_battery_full.
        let vars = (charge_full, charge_now, voltage_min_design, power_now);
        let secs_until_battery_full = match vars {
            (Some(charge_full), Some(charge_now), Some(voltage_min_design), Some(power_now)) => {
                let charge_delta = charge_full - charge_now;
                let hours = charge_delta * voltage_min_design / power_now;
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

        // Update prev_*.
        prev_ac_status = ac_status;
        prev_battery_percent = battery_percent;

        // Sleep until next iteration.
        thread::sleep(Duration::from_secs(1));
    }
}
