//! Transfer a listening socket over a private Unix datagram, preserving its
//! accept backlog while ownership moves between server processes.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use anyhow::{bail, Result};

pub fn receiver(path: &Path) -> Result<UnixDatagram> {
    use std::os::unix::fs::PermissionsExt;
    let socket = UnixDatagram::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    Ok(socket)
}

pub fn send(path: &Path, fd: RawFd) -> Result<()> {
    let socket = UnixDatagram::unbound()?;
    socket.connect(path)?;
    let mut byte = [1u8];
    let mut data = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    // usize storage gives cmsghdr the alignment required on every supported Unix.
    let mut control = [0usize; 8];
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut data;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as _) as _;
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as _) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        if libc::sendmsg(socket.as_raw_fd(), &message, 0) == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

pub fn receive(socket: &UnixDatagram) -> Result<OwnedFd> {
    let mut byte = [0u8];
    let mut data = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut control = [0usize; 8];
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut data;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control) as _;
        if libc::recvmsg(socket.as_raw_fd(), &mut message, 0) == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() || (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS
            || (*header).cmsg_len as usize != libc::CMSG_LEN(std::mem::size_of::<RawFd>() as _) as usize {
            bail!("missing web listener in deploy handover");
        }
        let fd = OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>()));
        if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transferred_listener_preserves_bound_address() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("fd.sock");
        let receiver = receiver(&path).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        send(&path, listener.as_raw_fd()).unwrap();
        let fd = receive(&receiver).unwrap();
        let inherited = std::net::TcpListener::from(fd);
        drop(listener);
        assert_eq!(inherited.local_addr().unwrap(), address);
        let client = std::net::TcpStream::connect(address).unwrap();
        let _accepted = inherited.accept().unwrap();
        drop(client);
    }
}
