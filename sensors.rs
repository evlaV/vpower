use libc::*;
use std::ffi::CStr;
use std::fs;
use std::mem::MaybeUninit;
use std::ptr;
use std::str::FromStr;

#[repr(C)]
struct sensors_bus_id {
    ty: c_short,
    nr: c_short,
}

#[repr(C)]
struct sensors_chip_name {
    prefix: *mut c_char,
    bus: sensors_bus_id,
    addr: c_int,
    path: *mut c_char,
}

#[repr(C)]
struct sensors_feature {
    name: *mut c_char,
    number: c_int,
    ty: c_int,
    first_subfeature: c_int,
    padding1: c_int,
}

#[repr(C)]
#[derive(Debug)]
struct sensors_subfeature {
    name: *mut c_char,
    number: c_int,
    ty: c_int,
    mapping: c_int,
    flags: c_uint,
}

#[link(name = "sensors")]
extern "C" {
    fn sensors_init(input: *mut FILE) -> c_int;
    fn sensors_cleanup();

    fn sensors_get_detected_chips(
        mat: *const sensors_chip_name,
        nr: *mut c_int,
    ) -> *const sensors_chip_name;

    fn sensors_get_features(
        name: *const sensors_chip_name,
        nr: *mut c_int,
    ) -> *const sensors_feature;

    fn sensors_get_subfeature(
        name: *const sensors_chip_name,
        feature: *const sensors_feature,
        ty: c_int,
    ) -> *const sensors_subfeature;

    fn sensors_get_value(
        name: *const sensors_chip_name,
        subfeat_nr: c_int,
        value: *mut c_double,
    ) -> c_int;
}

const SENSORS_FEATURE_IN: c_int = 0x00;
const SENSORS_FEATURE_CURR: c_int = 0x05;

const SENSORS_SUBFEATURE_IN_INPUT: c_int = SENSORS_FEATURE_IN << 8;
const SENSORS_SUBFEATURE_CURR_INPUT: c_int = SENSORS_FEATURE_CURR << 8;

unsafe fn get_chip(name: *const c_char) -> *const sensors_chip_name {
    let mut nr = 0;
    loop {
        let chip = sensors_get_detected_chips(ptr::null(), &mut nr);
        if chip.is_null() {
            return chip;
        }

        let chip = &*chip;
        if strcmp(chip.prefix, name) == 0 {
            return chip;
        }
    }
}

unsafe fn get_feature(chip: *const sensors_chip_name, feature_ty: c_int) -> *const sensors_feature {
    let mut nr = 0;
    loop {
        let feature = sensors_get_features(chip, &mut nr);
        if feature.is_null() {
            return feature;
        }

        let feature = &*feature;
        if feature.ty == feature_ty {
            return feature;
        }
    }
}

unsafe fn get_subfeature_num(
    chip: *const sensors_chip_name,
    feature_ty: c_int,
    subfeature_ty: c_int,
) -> Option<c_int> {
    let feature = get_feature(chip, feature_ty);
    if !feature.is_null() {
        let subfeature = sensors_get_subfeature(chip, feature, subfeature_ty);
        if !subfeature.is_null() {
            let subfeature = &*subfeature;
            return Some(subfeature.number);
        }
    }
    None
}

pub struct Sensors {
    initialized: bool,
    chip: *const sensors_chip_name,
    pdvl_subfeature_num: Option<c_int>, // PD contract voltage.
    pdam_subfeature_num: Option<c_int>, // PD contract current.
}

impl Sensors {
    pub fn new() -> Sensors {
        let mut sensors = Sensors {
            initialized: false,
            chip: ptr::null(),
            pdvl_subfeature_num: None,
            pdam_subfeature_num: None,
        };

        unsafe {
            sensors.initialized = sensors_init(ptr::null_mut()) == 0;
            if sensors.initialized {
                sensors.chip = get_chip("jupiter\0".as_ptr() as *const c_char);
                if !sensors.chip.is_null() {
                    sensors.pdvl_subfeature_num = get_subfeature_num(
                        sensors.chip,
                        SENSORS_FEATURE_IN,
                        SENSORS_SUBFEATURE_IN_INPUT,
                    );
                    sensors.pdam_subfeature_num = get_subfeature_num(
                        sensors.chip,
                        SENSORS_FEATURE_CURR,
                        SENSORS_SUBFEATURE_CURR_INPUT,
                    );
                }
            }
        }

        sensors
    }

    fn path(&self) -> Option<String> {
        if self.chip.is_null() {
            None
        } else {
            unsafe {
                let chip = &*self.chip;
                Some(CStr::from_ptr(chip.path).to_owned().into_string().unwrap())
            }
        }
    }

    // Firmware version.
    fn firmware_version(&self) -> Option<u32> {
        if let Some(path) = self.path() {
            let path = format!("{path}/firmware_version");
            if let Ok(string) = fs::read_to_string(&path) {
                if let Ok(val) = u32::from_str(string.trim()) {
                    return Some(val);
                }
            }
        }
        None
    }

    // PD contract status.
    pub fn pdcs(&self) -> Option<u8> {
        if let Some(path) = self.path() {
            let path = format!("{path}/pdcs");
            if let Ok(string) = fs::read_to_string(&path) {
                if let Ok(val) = u8::from_str(string.trim()) {
                    return Some(val);
                }
            }
        }
        None
    }

    // PD contract voltage (Volts).
    pub fn pdvl(&self) -> Option<f64> {
        // PDVL is bugged on certain BIOS's.
        match self.firmware_version() {
            None => return None,
            Some(firmware_version) => {
                if firmware_version <= 45096 {
                    return None;
                }
            }
        }

        if !self.chip.is_null() {
            if let Some(subfeature_num) = self.pdvl_subfeature_num {
                unsafe {
                    let mut val = MaybeUninit::uninit();
                    if sensors_get_value(self.chip, subfeature_num, val.as_mut_ptr()) == 0 {
                        return Some(val.assume_init());
                    }
                }
            }
        }
        None
    }

    // PD contract current (Amps).
    pub fn pdam(&self) -> Option<f64> {
        if !self.chip.is_null() {
            if let Some(subfeature_num) = self.pdam_subfeature_num {
                unsafe {
                    let mut val = MaybeUninit::uninit();
                    if sensors_get_value(self.chip, subfeature_num, val.as_mut_ptr()) == 0 {
                        return Some(val.assume_init());
                    }
                }
            }
        }
        None
    }
}

impl Drop for Sensors {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { sensors_cleanup() };
        }
    }
}
