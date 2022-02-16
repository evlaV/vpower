mod sensors;

use self::sensors::Sensors;
use serde::Deserialize;
use std::cmp::Ordering;
use std::fs;
use std::io;
use std::process::Command;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

#[derive(Deserialize)]
struct Config {
    request_shutdown_battery_percent: Option<f64>,
    force_shutdown_timeout_secs: Option<f64>,
}

fn read_battery_string(var_name: &str) -> String {
    let path = format!("/sys/class/power_supply/BAT1/{var_name}");
    match fs::read_to_string(&path) {
        Err(err) => panic!("read {path}: {err}"),
        Ok(string) => string.trim().to_owned(),
    }
}

fn read_battery_f64(var_name: &str) -> f64 {
    let path = format!("/sys/class/power_supply/BAT1/{var_name}");
    match fs::read_to_string(&path) {
        Err(err) => panic!("read {path}: {err}"),
        Ok(string) => match f64::from_str(string.trim()) {
            Err(err) => panic!("read {path}: {err}"),
            Ok(val) => {
                if !val.is_finite() {
                    panic!("read {path}: {val} is not finite");
                }
                val
            }
        },
    }
}
fn write_str(dir_path: &str, var_name: &str, val: &str) {
    if let Err(err) = fs::create_dir(dir_path) {
        if err.kind() != io::ErrorKind::AlreadyExists {
            panic!("mkdir {dir_path}: {err}");
        }
    }

    // Write to a temporary path first.
    let dot_path = format!("{dir_path}/.{var_name}");
    if let Err(err) = fs::write(&dot_path, format!("{val}\n")) {
        panic!("write {dot_path}: {err}");
    }

    // Then move into place for atomicity.
    let final_path = format!("{dir_path}/{var_name}");
    if let Err(err) = fs::rename(&dot_path, &final_path) {
        panic!("rename {dot_path} -> {final_path}: {err}");
    }
}

fn write_f64(dir_path: &str, var_name: &str, val: f64) {
    write_str(dir_path, var_name, &val.to_string())
}

fn main() {
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

    // Keep to heuristically determine if full, charging, or discharging.
    let mut prev_battery_percent = None;
    let mut full = false;

    // Start.
    println!("Running.");

    // Every second:
    loop {
        // Read battery variables.
        let charge_full = read_battery_f64("charge_full");
        let charge_now = read_battery_f64("charge_now");
        let current_now = read_battery_f64("current_now");
        let pdam = sensors.pdam();
        let pdcs = sensors.pdcs();
        let pdvl = sensors.pdvl();
        let status = read_battery_string("status");
        let voltage_min_design = read_battery_f64("voltage_min_design");
        let voltage_now = read_battery_f64("voltage_now");

        // Derive battery variables.
        let charge_shutdown = charge_full * (request_shutdown_battery_percent / 100.0);
        let power_now = voltage_now * current_now;

        // Calculate battery_percent.
        let battery_percent = charge_now / charge_full * 100.0;

        // Calculate secs_until_battery_full.
        let hours_until_battery_full = (charge_full - charge_now) * voltage_min_design / power_now;
        let secs_until_battery_full = hours_until_battery_full * 3600.0;

        // Calcuate secs_until_shutdown_request.
        let secs_until_shutdown_request = if charge_now > charge_shutdown {
            let charge_delta = charge_now - charge_shutdown;
            let hours_until_shutdown_request = charge_delta * voltage_min_design / power_now;
            hours_until_shutdown_request * 3600.0
        } else if status == "Not charging" || status == "Discharging" {
            0.0
        } else {
            1.0
        };

        // Calculate ac_status.
        let ac_status = if let (Some(pdcs), Some(pdvl), Some(pdam)) = (pdcs, pdvl, pdam) {
            let connected = (pdcs & (1 << 0)) != 0;
            let sink = (pdcs & (1 << 4)) == 0;
            if connected && sink {
                let pd_power = pdvl * pdam; // Watts.
                if pd_power > 0.0 && pd_power < 30.0 {
                    Some("Connected slow")
                } else {
                    Some("Connected")
                }
            } else {
                Some("Disconnected")
            }
        } else if status == "Full" || status == "Charging" {
            Some("Connected")
        } else if status == "Discharging" {
            Some("Disconnected")
        } else {
            None
        };

        // Update full.
        if status == "Full" || (status == "Charging" && battery_percent >= 99.5) {
            full = true;
        } else if status == "Discharging" || battery_percent < 95.0 {
            full = false;
        }

        // Calculate battery_status.
        let battery_status = if full {
            Some("Full")
        } else if let Some(prev_battery_percent) = prev_battery_percent {
            match battery_percent.partial_cmp(&prev_battery_percent) {
                Some(Ordering::Greater) => Some("Charging"),
                Some(Ordering::Less) => Some("Discharging"),
                _ => None,
            }
        } else {
            None
        }
        .or_else(|| {
            if status == "Charging" {
                Some("Charging")
            } else if status == "Discharging" {
                Some("Discharging")
            } else {
                None
            }
        });

        // Write to /run/vpower/*
        let dir_path = "/run/vpower";
        write_f64(dir_path, "battery_percent", battery_percent);
        let val = secs_until_battery_full;
        write_f64(dir_path, "secs_until_battery_full", val);
        let val = secs_until_shutdown_request;
        write_f64(dir_path, "secs_until_shutdown_request", val);

        if let Some(ac_status) = ac_status {
            write_str(dir_path, "ac_status", ac_status);
        }

        if let Some(battery_status) = battery_status {
            write_str(dir_path, "battery_status", battery_status);
        }

        // Force shutdown after timeout.
        if secs_until_shutdown_request == 0.0 {
            println!("Reached {request_shutdown_battery_percent}% battery.");
            println!("Forcing shutdown in {force_shutdown_timeout_secs} seconds.");
            thread::sleep(Duration::from_secs_f64(force_shutdown_timeout_secs));

            println!("Shutting down now.");
            Command::new("poweroff").output().unwrap();
            return;
        }

        // Update prev_battery_percent.
        prev_battery_percent = Some(battery_percent);

        // Sleep until next iteration.
        thread::sleep(Duration::from_secs(1));
    }
}
