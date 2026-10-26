//! Nonblocking stdio for the SSH worker. Tokio's blocking stdin adapter can
//! keep a runtime alive after cancellation; these owned descriptors cannot.
use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

pub struct Pipe(AsyncFd<File>);

impl Pipe {
    pub fn input() -> io::Result<Self> {
        Self::from_fd(io::stdin().as_fd())
    }
    pub fn output() -> io::Result<Self> {
        Self::from_fd(io::stdout().as_fd())
    }

    fn from_fd(inherited: BorrowedFd<'_>) -> io::Result<Self> {
        // SSH can pass root-owned pipes to an unprivileged worker. The
        // inherited descriptor is usable, but reopening /dev/stdin or
        // /dev/stdout checks the pipe's permissions and can fail.
        let owned = inherited.try_clone_to_owned()?;
        let fd = owned.as_raw_fd();
        // SAFETY: owned keeps this descriptor valid through both calls.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        // Preserve the other flags. Duplicates share these file-status flags;
        // the host service is the sole reader and writer of its stdio.
        // SAFETY: owned still owns the valid descriptor.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(AsyncFd::new(File::from(owned))?))
    }
}

impl AsyncRead for Pipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut ready = std::task::ready!(self.0.poll_read_ready(cx))?;
            match ready.try_io(|fd| fd.get_ref().read(buffer.initialize_unfilled())) {
                Ok(Ok(size)) => {
                    buffer.advance(size);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_) => continue,
            }
        }
    }
}

impl AsyncWrite for Pipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut ready = std::task::ready!(self.0.poll_write_ready(cx))?;
            match ready.try_io(|fd| fd.get_ref().write(buffer)) {
                Ok(result) => return Poll::Ready(result),
                Err(_) => continue,
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixStream, time::Duration};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn inherited_pipe_remains_usable_when_path_access_is_denied() {
        use std::os::fd::{FromRawFd, OwnedFd};
        let mut descriptors = [-1; 2];
        // SAFETY: pipe2 writes exactly two descriptors to this array.
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // SAFETY: pipe2 returned distinct valid descriptors; each is owned once.
        let read = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: this is the second descriptor returned by pipe2.
        let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        // SAFETY: read owns a valid descriptor. This changes only this test pipe.
        assert_eq!(unsafe { libc::fchmod(read.as_raw_fd(), 0) }, 0);
        // Root can bypass these permissions; all other users reproduce the SSH
        // failure when reopening a pipe by path.
        // SAFETY: geteuid takes no arguments and has no pointer preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let error = File::open(format!("/proc/self/fd/{}", read.as_raw_fd())).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
        let mut input = Pipe::from_fd(read.as_fd()).unwrap();
        let mut output = Pipe::from_fd(write.as_fd()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            output.write_all(b"hello").await.unwrap();
            let mut bytes = [0; 5];
            input.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"hello");
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn duplicated_descriptors_read_write_and_leave_the_original_open() {
        let (inherited, peer) = UnixStream::pair().unwrap();
        let mut input = Pipe::from_fd(inherited.as_fd()).unwrap();
        let mut output = Pipe::from_fd(peer.as_fd()).unwrap();
        let fd = input.0.get_ref().as_raw_fd();
        // SAFETY: input owns this descriptor for both queries.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        // SAFETY: input still owns the descriptor.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            output.write_all(b"hello").await.unwrap();
            let mut bytes = [0; 5];
            input.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"hello");
            input.write_all(b"reply").await.unwrap();
            output.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"reply");
        })
        .await
        .unwrap();
        drop(input);
        let mut inherited = inherited;
        output.write_all(b"open").await.unwrap();
        let mut bytes = [0; 4];
        inherited.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"open");
    }
}
