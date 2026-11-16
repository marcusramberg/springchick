//! Password checks through PAM, for unlocking.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;

/// `/etc/pam.d/springchick`; without it PAM falls back to `other`, which most
/// distributions make deny.
const SERVICE: &CStr = c"springchick";

const PAM_SUCCESS: c_int = 0;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_BUF_ERR: c_int = 5;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(
    c_int,
    *mut *const PamMessage,
    *mut *mut PamResponse,
    *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        handle: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_acct_mgmt(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
}

struct Credentials {
    user: CString,
    password: CString,
}

/// Answers PAM's prompts: the password for hidden ones, the user name for
/// visible ones.
unsafe extern "C" fn converse(
    count: c_int,
    messages: *mut *const PamMessage,
    responses: *mut *mut PamResponse,
    data: *mut c_void,
) -> c_int {
    let credentials = &*(data as *const Credentials);
    // PAM frees the responses and their strings with free().
    let replies = libc::calloc(count as usize, size_of::<PamResponse>()) as *mut PamResponse;
    if replies.is_null() {
        return PAM_BUF_ERR;
    }
    for i in 0..count as usize {
        let answer = match (**messages.add(i)).msg_style {
            PAM_PROMPT_ECHO_OFF => libc::strdup(credentials.password.as_ptr()),
            PAM_PROMPT_ECHO_ON => libc::strdup(credentials.user.as_ptr()),
            _ => ptr::null_mut(),
        };
        (*replies.add(i)).resp = answer;
    }
    *responses = replies;
    PAM_SUCCESS
}

/// Checks `password` for the user sc-bar runs as. Blocks, for seconds when it
/// is wrong (pam_faildelay): call it off the main thread.
pub fn authenticate(password: &str) -> bool {
    let Some(user) = current_user() else {
        return false;
    };
    let Ok(password) = CString::new(password) else {
        return false;
    };
    let credentials = Credentials { user, password };
    let conv = PamConv {
        conv: converse,
        appdata_ptr: &credentials as *const Credentials as *mut c_void,
    };
    // SAFETY: `credentials` and `conv` outlive the handle, which is ended
    // before they go.
    unsafe {
        let mut handle = ptr::null_mut();
        let mut status = pam_start(
            SERVICE.as_ptr(),
            credentials.user.as_ptr(),
            &conv,
            &mut handle,
        );
        if status == PAM_SUCCESS {
            status = pam_authenticate(handle, 0);
        }
        if status == PAM_SUCCESS {
            status = pam_acct_mgmt(handle, 0);
        }
        if !handle.is_null() {
            pam_end(handle, status);
        }
        status == PAM_SUCCESS
    }
}

fn current_user() -> Option<CString> {
    // SAFETY: getpwuid's static entry is copied before anything else can ask.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        (!entry.is_null()).then(|| CStr::from_ptr((*entry).pw_name).to_owned())
    }
}
