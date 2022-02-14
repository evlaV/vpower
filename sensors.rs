use libc::*;
use std::mem::MaybeUninit;
use std::ptr;

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

    fn sensors_parse_chip_name(orig_name: *const c_char, res: *mut sensors_chip_name) -> c_int;
    fn sensors_free_chip_name(chip: *mut sensors_chip_name);

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
    chip: Option<sensors_chip_name>,
    pdvl_subfeature_num: Option<c_int>, // PD contract voltage.
    pdam_subfeature_num: Option<c_int>, // PD contract current.
}

impl Sensors {
    pub fn new() -> Sensors {
        let mut sensors = Sensors {
            initialized: false,
            chip: None,
            pdvl_subfeature_num: None,
            pdam_subfeature_num: None,
        };

        unsafe {
            sensors.initialized = sensors_init(ptr::null_mut()) == 0;
            if sensors.initialized {
                let mut chip = MaybeUninit::uninit();
                if sensors_parse_chip_name(
                    "jupiter-isa-0000\0".as_ptr() as _,
                    chip.as_mut_ptr(),
                ) == 0
                {
                    sensors.chip = Some(chip.assume_init());
                    if let Some(jupiter_isa) = &sensors.chip {
                        sensors.pdvl_subfeature_num = get_subfeature_num(
                            jupiter_isa,
                            SENSORS_FEATURE_IN,
                            SENSORS_SUBFEATURE_IN_INPUT,
                        );
                        sensors.pdam_subfeature_num = get_subfeature_num(
                            jupiter_isa,
                            SENSORS_FEATURE_CURR,
                            SENSORS_SUBFEATURE_CURR_INPUT,
                        );
                    }
                }
            }
        }

        sensors
    }

    // PD contract voltage.
    pub fn pdvl(&self) -> Option<f64> {
        if let Some(chip) = &self.chip {
            if let Some(subfeature_num) = self.pdvl_subfeature_num {
                unsafe {
                    let mut val = MaybeUninit::uninit();
                    let err = sensors_get_value(chip, subfeature_num, val.as_mut_ptr());
                    if err == 0 {
                        return Some(val.assume_init());
                    }
                }
            }
        }
        None
    }

    // PD contract current.
    pub fn pdam(&self) -> Option<f64> {
        if let Some(chip) = &self.chip {
            if let Some(subfeature_num) = self.pdam_subfeature_num {
                unsafe {
                    let mut val = MaybeUninit::uninit();
                    let err = sensors_get_value(chip, subfeature_num, val.as_mut_ptr());
                    if err == 0 {
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
        unsafe {
            if let Some(chip) = &mut self.chip {
                sensors_free_chip_name(chip)
            }

            if self.initialized {
                sensors_cleanup();
            }
        }
    }
}
