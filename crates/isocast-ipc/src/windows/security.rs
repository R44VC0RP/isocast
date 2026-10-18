use std::io;
use std::mem::{align_of, offset_of, size_of};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    LocalFree, WAIT_FAILED, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, EqualSid, GetLengthSid,
    GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor, IsValidSid,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR, SID, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use super::{PIPE_BUFFER_SIZE, Pipe, closed, last_error, owned, win_error};

// GetTokenInformation writes a TOKEN_USER followed by its SID. usize storage
// provides the header's alignment; moving the Vec never moves its allocation.
pub(super) struct User {
    data: Vec<usize>,
    sid_offset: usize,
    sid_length: usize,
}

// Retain the process object while the stream exists, preventing PID reuse after
// authentication. This is process-token inspection, never thread impersonation.
pub(super) struct Peer {
    _process: OwnedHandle,
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // GetSecurityInfo allocates its self-relative descriptor with LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

impl User {
    pub(super) fn current() -> io::Result<Self> {
        Self::of_process(unsafe { GetCurrentProcess() })
    }

    fn of_process(process: HANDLE) -> io::Result<Self> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(last_error());
        }
        let token = unsafe { owned(token)? };
        let mut size = 0;
        let success = unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size)
        };
        if success == 0 && unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(last_error());
        }
        if (size as usize) < size_of::<TOKEN_USER>() {
            return Err(io::Error::other(
                "Windows returned an invalid token user size",
            ));
        }
        let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                data.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        } == 0
        {
            return Err(last_error());
        }
        let invalid = || io::Error::other("Windows returned an invalid token user SID");
        let length = size as usize;
        if length < size_of::<TOKEN_USER>() || length > data.len() * size_of::<usize>() {
            return Err(invalid());
        }
        // SAFETY: GetTokenInformation initialized the aligned TOKEN_USER header.
        let sid = unsafe { (*data.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let sid_offset = (sid as usize)
            .checked_sub(data.as_ptr() as usize)
            .ok_or_else(invalid)?;
        let header_length = offset_of!(SID, SubAuthority);
        if sid_offset < size_of::<TOKEN_USER>()
            || !sid_offset.is_multiple_of(align_of::<SID>())
            || sid_offset > length.saturating_sub(header_length)
        {
            return Err(invalid());
        }
        // Validate the variable-sized tail BEFORE asking SID APIs to inspect it.
        // The subauthority count is a byte within the now-bounds-checked header.
        let count = unsafe {
            *data
                .as_ptr()
                .cast::<u8>()
                .add(sid_offset + offset_of!(SID, SubAuthorityCount))
        };
        let sid_length = header_length + usize::from(count) * size_of::<u32>();
        if sid_length > length - sid_offset || unsafe { IsValidSid(sid) } == 0 {
            return Err(invalid());
        }
        if unsafe { GetLengthSid(sid) } as usize != sid_length {
            return Err(invalid());
        }
        Ok(Self {
            data,
            sid_offset,
            sid_length,
        })
    }

    fn sid(&self) -> PSID {
        // SAFETY: of_process validated the SID's complete span in this allocation.
        unsafe { self.data.as_ptr().cast::<u8>().add(self.sid_offset) as PSID }
    }

    pub(super) fn sid_bytes(&self) -> &[u8] {
        // SAFETY: immutable, validated SID bytes owned by self. The borrow cannot
        // outlive the token storage, and excludes TOKEN_USER's pointer/padding.
        unsafe { std::slice::from_raw_parts(self.sid().cast(), self.sid_length) }
    }

    pub(super) fn verify_peer(&self, pipe: &Pipe) -> io::Result<Peer> {
        if !pipe.server {
            // Also reject a pre-created pipe owned by another user. Unlike a
            // numeric PID, the object's owner cannot become a different process
            // through PID recycling before OpenProcess gets a lifetime pin.
            self.verify_owner(pipe)?;
        }
        let pid = pipe.peer_pid()?;
        let process = unsafe {
            owned(OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            ))?
        };
        let user = Self::of_process(process.as_raw_handle())?;
        if unsafe { EqualSid(self.sid(), user.sid()) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local pipe peer belongs to a different Windows user",
            ));
        }
        // Fail closed if the process died during authentication. Holding its
        // handle prevents this PID being reused for the lifetime of the stream.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => {}
            WAIT_FAILED => return Err(last_error()),
            _ => return Err(closed()),
        }
        if pipe.peer_pid()? != pid {
            return Err(closed());
        }
        Ok(Peer { _process: process })
    }

    fn verify_owner(&self, pipe: &Pipe) -> io::Result<()> {
        let mut owner = null_mut();
        let mut descriptor = null_mut();
        let code = unsafe {
            GetSecurityInfo(
                pipe.raw(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if code != 0 {
            return Err(win_error(code));
        }
        let _descriptor = SecurityDescriptor(descriptor);
        if owner.is_null() || unsafe { EqualSid(self.sid(), owner) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local pipe is owned by a different Windows user",
            ));
        }
        Ok(())
    }

    pub(super) fn create_pipe(&self, name: &[u16], first: bool) -> io::Result<Pipe> {
        // Exactly one allow ACE, for the process's actual user SID, not the
        // broad default named-pipe ACL (Everyone/Anonymous/Administrators).
        let acl_size = size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>()
            + unsafe { GetLengthSid(self.sid()) } as usize;
        let mut storage = vec![0u32; acl_size.div_ceil(size_of::<u32>())];
        let acl = storage.as_mut_ptr().cast::<ACL>();
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_ptr = (&raw mut descriptor).cast();
        // SAFETY: aligned ACL storage with room for the header, ACE and full
        // SID; the descriptor, SID and ACL remain alive through CreateNamedPipe.
        if unsafe { InitializeAcl(acl, acl_size as u32, ACL_REVISION) } == 0
            || unsafe { AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, self.sid()) } == 0
            || unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) }
                == 0
            || unsafe { SetSecurityDescriptorOwner(descriptor_ptr, self.sid(), 0) } == 0
            || unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl, 0) } == 0
            || unsafe {
                SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
            } == 0
        {
            return Err(last_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor_ptr,
            bInheritHandle: 0,
        };
        let first_flag = if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first_flag,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0,
                &attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let code = unsafe { GetLastError() };
            // FIRST_PIPE_INSTANCE reports ACCESS_DENIED for an occupied name,
            // including one pre-created by another user. Never attach to it.
            return Err(if first && code == ERROR_ACCESS_DENIED {
                io::Error::new(io::ErrorKind::AddrInUse, win_error(code))
            } else {
                win_error(code)
            });
        }
        Ok(Pipe {
            handle: unsafe { owned(handle)? },
            server: true,
        })
    }
}
