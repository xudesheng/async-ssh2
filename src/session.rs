use crate::{
    agent::Agent, channel::Channel, listener::Listener, sftp::Sftp, util::run_ssh2_fn, Error,
};
use async_io::Async;
use ssh2::{
    self, DisconnectCode, HashType, HostKeyType, KeyboardInteractivePrompt, KnownHosts, MethodType,
    ScpFileStat, BlockDirections
};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, RawSocket};
use std::{
    convert::From,
    net::{TcpStream,TcpListener},
    path::Path,
    sync::Arc,
};
use futures::{AsyncReadExt, AsyncWriteExt, FutureExt, select};
/// See [`Session`](ssh2::Session).
pub struct Session {
    inner: ssh2::Session,
    stream: Option<Arc<Async<TcpStream>>>,
}

#[cfg(unix)]
struct RawFdWrapper(RawFd);

#[cfg(unix)]
impl AsRawFd for RawFdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[cfg(windows)]
struct RawSocketWrapper(RawSocket);

#[cfg(windows)]
impl AsRawSocket for RawSocketWrapper {
    fn as_raw_socket(&self) -> RawSocket {
        self.0
    }
}

impl Session {
    /// See [`new`](ssh2::Session::new).
    pub fn new() -> Result<Session, Error> {
        let session = ssh2::Session::new()?;
        session.set_blocking(false);

        Ok(Self {
            inner: session,
            stream: None,
        })
    }

    /// See [`set_banner`](ssh2::Session::set_banner).
    pub async fn set_banner(&self, banner: &str) -> Result<(), Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner.set_banner(banner)
        })
        .await
    }

    /// See [`set_allow_sigpipe`](ssh2::Session::set_allow_sigpipe).
    pub fn set_allow_sigpipe(&self, block: bool) {
        self.inner.set_allow_sigpipe(block)
    }

    /// See [`set_allow_sigpipe`](ssh2::Session::set_compress).
    pub fn set_compress(&self, compress: bool) {
        self.inner.set_compress(compress)
    }

    /// See [`is_blocking`](ssh2::Session::is_blocking).
    pub fn is_blocking(&self) -> bool {
        self.inner.is_blocking()
    }

    /// See [`set_timeout`](ssh2::Session::set_timeout).
    pub fn set_timeout(&self, timeout_ms: u32) {
        self.inner.set_timeout(timeout_ms)
    }

    /// See [`timeout`](ssh2::Session::timeout).
    pub fn timeout(&self) -> u32 {
        self.inner.timeout()
    }

    /// See [`handshake`](ssh2::Session::handshake).
    pub async fn handshake(&mut self) -> Result<(), Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner.clone().handshake()
        })
        .await
    }

    /// Sets the tcp stream for the underlying `ssh2` lib.
    ///
    /// ```rust,no_run
    /// use async_ssh2::Session;
    /// use std::net::{ToSocketAddrs, SocketAddr, TcpStream};
    /// use async_io::Async;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut addr = SocketAddr::from(([127, 0, 0, 1], 22)).to_socket_addrs().unwrap();
    ///     let stream = Async::<TcpStream>::connect(addr.next().unwrap()).await.unwrap();
    ///     let mut sess = async_ssh2::Session::new().unwrap();
    ///     sess.set_tcp_stream(stream).unwrap();
    /// }
    /// ```
    pub fn set_tcp_stream(&mut self, stream: Async<TcpStream>) -> Result<(), Error> {
        #[cfg(unix)]
        {
            let raw_fd = RawFdWrapper(stream.as_raw_fd());
            self.inner.set_tcp_stream(raw_fd);
        }
        #[cfg(windows)]
        {
            let raw_socket = RawSocketWrapper(stream.as_raw_socket());
            self.inner.set_tcp_stream(raw_socket);
        }
        self.stream = Some(Arc::new(stream));
        Ok(())
    }

    pub async fn proxy_session(&self, target_ip:&str, target_port:u16,src:Option<(&str,u16)>) -> Result<Session,Error>{
        let mut bastion_channel = self.channel_direct_tcpip(target_ip,target_port,src).await?;
        let (forward_stream_s, mut forward_stream_r) = {
            cfg_if::cfg_if! {
                if #[cfg(unix)] {
                    let dir = tempdir();
                    let path = dir.path().join("ssh_channel_direct_tcpip");
                    let listener = Async::<UnixListener>::bind(&path)?;
                    let stream_s = Async::<UnixStream>::accept(&path).await?;
                }else{
                    let listen_addr = TcpListener::bind("localhost:0").unwrap().local_addr().unwrap();
                    let listener = Async::<TcpListener>::bind(listen_addr)?;
                    let stream_s = Async::<TcpStream>::connect(listen_addr).await?;
                }
            }
            let (stream_r, _) = listener.accept().await?;
            (stream_s, stream_r)
        };

        let _backend_task = tokio::spawn(async move {
            let mut buf_bastion_channel = vec![0; 2048];
            let mut buf_forward_stream_r = vec![0; 2048];
            
            loop {
                select! {
                    ret_forward_stream_r = forward_stream_r.read(&mut buf_forward_stream_r).fuse() => match ret_forward_stream_r{
                        Ok(n) if n == 0 => {
                            println!("forward_stream_r closed.");
                            break;
                        },
                        Ok(n) => {
                            println!("forward_stream_r read {} bytes.",n);
                            bastion_channel.write_all(&buf_forward_stream_r[..n]).await.map(|_| ()).map_err(|err| {
                                eprintln!("bastion_channel write failed, err: {:?}",err);
                                err
                            })?;
                        },
                        Err(err) => {
                            eprintln!("forward_stream_r read failed, err: {:?}",err);
                            return Err(err);
                        }
                    },
                    ret_bastion_channel = bastion_channel.read(&mut buf_bastion_channel).fuse() => match ret_bastion_channel {
                        Ok(n) if n == 0 => {
                            println!("bastion_channel closed.");
                            break;
                        },
                        Ok(n) => {
                            println!("bastion_channel read {} bytes.",n);
                            forward_stream_r.write_all(&buf_bastion_channel[..n]).await.map(|_| ()).map_err(|err| {
                                eprintln!("forward_stream_s write failed, err: {:?}",err);
                                err
                            })?;
                        },
                        Err(err) => {
                            eprintln!("bastion_channel read failed, err: {:?}",err);
                            return Err(err);
                        }
                    },
                }
            }
            
            Ok(())
        });

        let mut child_sess = Session::new()?;
        child_sess.set_tcp_stream(forward_stream_s)?;
        Ok(child_sess)
    }

    /// See [`userauth_password`](ssh2::Session::userauth_password).
    pub async fn userauth_password(&self, username: &str, password: &str) -> Result<(), Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner.userauth_password(username, password)
        })
        .await
    }

    /// See [`userauth_keyboard_interactive`](ssh2::Session::userauth_keyboard_interactive).
    pub fn userauth_keyboard_interactive<P: KeyboardInteractivePrompt>(
        &self,
        _username: &str,
        _prompter: &mut P,
    ) -> Result<(), Error> {
        unimplemented!();
    }

    /// See [`userauth_agent`](ssh2::Session::userauth_agent).
    pub async fn userauth_agent(&self, username: &str) -> Result<(), Error> {
        let mut agent = self.agent()?;
        agent.connect().await?;
        agent.list_identities()?;
        let identities = agent.identities()?;
        let identity = match identities.get(0) {
            Some(identity) => identity,
            None => return Err(Error::from(ssh2::Error::from_errno(ssh2::ErrorCode::Session(-4)))),
        };
        agent.userauth(username, &identity).await
    }

    /// See [`userauth_pubkey_file`](ssh2::Session::userauth_pubkey_file).
    pub async fn userauth_pubkey_file(
        &self,
        username: &str,
        pubkey: Option<&Path>,
        privatekey: &Path,
        passphrase: Option<&str>,
    ) -> Result<(), Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner
                .userauth_pubkey_file(username, pubkey, privatekey, passphrase)
        })
        .await
    }

    /// See [`userauth_pubkey_memory`](ssh2::Session::userauth_pubkey_memory).
    #[cfg(unix)]
    pub async fn userauth_pubkey_memory(
        &self,
        username: &str,
        pubkeydata: Option<&str>,
        privatekeydata: &str,
        passphrase: Option<&str>,
    ) -> Result<(), Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner
                .userauth_pubkey_memory(username, pubkeydata, privatekeydata, passphrase)
        })
        .await
    }

    /// See [`userauth_hostbased_file`](ssh2::Session::userauth_hostbased_file).
    #[allow(missing_docs)]
    pub async fn userauth_hostbased_file(
        &self,
        username: &str,
        publickey: &Path,
        privatekey: &Path,
        passphrase: Option<&str>,
        hostname: &str,
        local_username: Option<&str>,
    ) -> Result<(), Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner.userauth_hostbased_file(
                username,
                publickey,
                privatekey,
                passphrase,
                hostname,
                local_username,
            )
        })
        .await
    }

    /// See [`authenticated`](ssh2::Session::authenticated).
    pub fn authenticated(&self) -> bool {
        self.inner.authenticated()
    }

    /// See [`auth_methods`](ssh2::Session::auth_methods).
    pub async fn auth_methods(&self, username: &str) -> Result<&str, Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner.auth_methods(username)
        })
        .await
    }

    /// See [`method_pref`](ssh2::Session::method_pref).
    pub fn method_pref(&self, method_type: MethodType, prefs: &str) -> Result<(), Error> {
        self.inner.method_pref(method_type, prefs)?;
        Ok(())
    }

    /// See [`methods`](ssh2::Session::methods).
    pub fn methods(&self, method_type: MethodType) -> Option<&str> {
        self.inner.methods(method_type)
    }

    /// See [`supported_algs`](ssh2::Session::supported_algs).
    pub fn supported_algs(&self, method_type: MethodType) -> Result<Vec<&'static str>, Error> {
        self.inner.supported_algs(method_type).map_err(From::from)
    }

    /// See [`agent`](ssh2::Session::agent).
    pub fn agent(&self) -> Result<Agent, Error> {
        let agent = self.inner.agent()?;
        Ok(Agent::new(agent, self.inner.clone(), self.stream.as_ref().unwrap().clone()))
    }

    /// See [`known_hosts`](ssh2::Session::known_hosts).
    pub fn known_hosts(&self) -> Result<KnownHosts, Error> {
        self.inner.known_hosts().map_err(From::from)
    }

    /// See [`channel_session`](ssh2::Session::channel_session).
    pub async fn channel_session(&self) -> Result<Channel, Error> {
        let channel = run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner.channel_session()
        })
        .await?;
        Ok(Channel::new(channel, self.inner.clone(), self.stream.as_ref().unwrap().clone()))
    }

    /// See [`channel_direct_tcpip`](ssh2::Session::channel_direct_tcpip).
    pub async fn channel_direct_tcpip(
        &self,
        host: &str,
        port: u16,
        src: Option<(&str, u16)>,
    ) -> Result<Channel, Error> {
        let channel = run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner.channel_direct_tcpip(host, port, src)
        })
        .await?;
        Ok(Channel::new(channel, self.inner.clone(), self.stream.as_ref().unwrap().clone()))
    }

    /// See [`channel_forward_listen`](ssh2::Session::channel_forward_listen).
    pub async fn channel_forward_listen(
        &self,
        remote_port: u16,
        host: Option<&str>,
        queue_maxsize: Option<u32>,
    ) -> Result<(Listener, u16), Error> {
        let (listener, port) = run_ssh2_fn(self.stream.as_ref().unwrap(), &self.inner, || {
            self.inner
                .channel_forward_listen(remote_port, host, queue_maxsize)
        })
        .await?;
        Ok((
            Listener::new(listener, self.inner.clone(), self.stream.as_ref().unwrap().clone()),
            port,
        ))
    }

    /// See [`scp_recv`](ssh2::Session::scp_recv).
    pub async fn scp_recv(&self, path: &Path) -> Result<(Channel, ScpFileStat), Error> {
        let (channel, file_stat) =
            run_ssh2_fn(self.stream.as_ref().unwrap(),  &self.inner, || self.inner.scp_recv(path)).await?;
        Ok((
            Channel::new(channel, self.inner.clone(), self.stream.as_ref().unwrap().clone()),
            file_stat,
        ))
    }

    /// See [`scp_send`](ssh2::Session::scp_send).
    pub async fn scp_send(
        &self,
        remote_path: &Path,
        mode: i32,
        size: u64,
        times: Option<(u64, u64)>,
    ) -> Result<Channel, Error> {
        let channel = run_ssh2_fn(self.stream.as_ref().unwrap(),  &self.inner, || {
            self.inner.scp_send(remote_path, mode, size, times)
        })
        .await?;
        Ok(Channel::new(channel, self.inner.clone(), self.stream.as_ref().unwrap().clone()))
    }

    /// See [`sftp`](ssh2::Session::sftp).
    pub async fn sftp(& self) -> Result<Sftp, Error> {
        let sftp = run_ssh2_fn(self.stream.as_ref().unwrap(),  &self.inner, || self.inner.sftp()).await?;
        Ok(Sftp::new(sftp, self.inner.clone(), self.stream.as_ref().unwrap().clone()))
    }

    /// See [`channel_open`](ssh2::Session::channel_open).
    pub async fn channel_open(
        &self,
        channel_type: &str,
        window_size: u32,
        packet_size: u32,
        message: Option<&str>,
    ) -> Result<Channel, Error> {
        let channel = run_ssh2_fn(self.stream.as_ref().unwrap(),  &self.inner, || {
            self.inner
                .channel_open(channel_type, window_size, packet_size, message)
        })
        .await?;
        Ok(Channel::new(channel, self.inner.clone(), self.stream.as_ref().unwrap().clone()))
    }

    /// See [`banner`](ssh2::Session::banner).
    pub fn banner(&self) -> Option<&str> {
        self.inner.banner()
    }

    /// See [`banner_bytes`](ssh2::Session::banner_bytes).
    pub fn banner_bytes(&self) -> Option<&[u8]> {
        self.inner.banner_bytes()
    }

    /// See [`host_key`](ssh2::Session::host_key).
    pub fn host_key(&self) -> Option<(&[u8], HostKeyType)> {
        self.inner.host_key()
    }

    /// See [`host_key_hash`](ssh2::Session::host_key_hash).
    pub fn host_key_hash(&self, hash: HashType) -> Option<&[u8]> {
        self.inner.host_key_hash(hash)
    }

    /// See [`set_keepalive`](ssh2::Session::set_keepalive).
    pub fn set_keepalive(&self, want_reply: bool, interval: u32) {
        self.inner.set_keepalive(want_reply, interval)
    }

    /// See [`keepalive_send`](ssh2::Session::keepalive_send).
    pub async fn keepalive_send(&self) -> Result<u32, Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(),  &self.inner, || {
            self.inner.keepalive_send()
        })
        .await
    }

    /// See [`disconnect`](ssh2::Session::disconnect).
    pub async fn disconnect(
        &self,
        reason: Option<DisconnectCode>,
        description: &str,
        lang: Option<&str>,
    ) -> Result<(), Error> {
        run_ssh2_fn(self.stream.as_ref().unwrap(),  &self.inner, || {
            self.inner.disconnect(reason, description, lang)
        })
        .await
    }

    /// See [`block_directions`](ssh2::Session::block_directions).
    pub fn block_directions(&self) -> BlockDirections {
        self.inner.block_directions()
    }

    /// See [`trace`](ssh2::Session::trace).
    pub fn trace(&self, bitmask: ssh2::TraceFlags) {
        self.inner.trace(bitmask);
    }
}

#[cfg(unix)]
impl AsRawFd for Session {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for Session {
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}
