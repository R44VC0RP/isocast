use std::ffi::c_void;
use std::mem::zeroed;

use crate::error::{Error, Result};
use crate::util::{VersionInfo, c_string};

pub(crate) fn check_encoder(status: i32) -> Result<()> {
    if status == libxaac_sys::IA_NO_ERROR as i32 {
        Ok(())
    } else {
        Err(Error::EncoderError(status))
    }
}

pub(crate) fn encoder_version() -> Result<VersionInfo> {
    let mut version: libxaac_sys::ixheaace_version = unsafe { zeroed() };
    let status = unsafe {
        libxaac_sys::ixheaace_get_lib_id_strings((&mut version as *mut _) as *mut c_void)
    };
    check_encoder(status)?;
    Ok(VersionInfo {
        name: c_string(version.p_lib_name),
        version: c_string(version.p_version_num),
    })
}
