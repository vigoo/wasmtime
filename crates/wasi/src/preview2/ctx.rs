use super::clocks::host::{monotonic_clock, wall_clock};
use crate::preview2::{
    clocks::{self, HostMonotonicClock, HostWallClock},
    filesystem::{Dir, TableFsExt},
    pipe, random, stdio,
    stdio::{StdioInput, StdioOutput},
    stream::{HostInputStream, HostOutputStream, TableStreamExt},
    DirPerms, FilePerms, IsATTY, Table,
};
use cap_rand::{Rng, RngCore, SeedableRng};
use cap_std::ipnet::{self, IpNet};
use cap_std::net::Pool;
use cap_std::{ambient_authority, AmbientAuthority};
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr};

pub struct WasiCtxBuilder {
    stdin: (Box<dyn HostInputStream>, IsATTY),
    stdout: (Box<dyn HostOutputStream>, IsATTY),
    stderr: (Box<dyn HostOutputStream>, IsATTY),
    env: Vec<(String, String)>,
    args: Vec<String>,
    preopens: Vec<(Dir, String)>,

    pool: Pool,
    random: Box<dyn RngCore + Send + Sync>,
    insecure_random: Box<dyn RngCore + Send + Sync>,
    insecure_random_seed: u128,
    wall_clock: Box<dyn HostWallClock + Send + Sync>,
    monotonic_clock: Box<dyn HostMonotonicClock + Send + Sync>,
    built: bool,
}

impl WasiCtxBuilder {
    /// Creates a builder for a new context with default parameters set.
    ///
    /// The current defaults are:
    ///
    /// * stdin is closed
    /// * stdout and stderr eat all input but it doesn't go anywhere
    /// * no env vars
    /// * no arguments
    /// * no preopens
    /// * clocks use the host implementation of wall/monotonic clocks
    /// * RNGs are all initialized with random state and suitable generator
    ///   quality to satisfy the requirements of WASI APIs.
    ///
    /// These defaults can all be updated via the various builder configuration
    /// methods below.
    ///
    /// Note that each builder can only be used once to produce a [`WasiCtx`].
    /// Invoking the [`build`](WasiCtxBuilder::build) method will panic on the
    /// second attempt.
    pub fn new() -> Self {
        // For the insecure random API, use `SmallRng`, which is fast. It's
        // also insecure, but that's the deal here.
        let insecure_random = Box::new(cap_rand::rngs::SmallRng::from_entropy());

        // For the insecure random seed, use a `u128` generated from
        // `thread_rng()`, so that it's not guessable from the insecure_random
        // API.
        let insecure_random_seed =
            cap_rand::thread_rng(cap_rand::ambient_authority()).gen::<u128>();
        Self {
            stdin: (Box::new(pipe::ClosedInputStream), IsATTY::No),
            stdout: (Box::new(pipe::SinkOutputStream), IsATTY::No),
            stderr: (Box::new(pipe::SinkOutputStream), IsATTY::No),
            env: Vec::new(),
            args: Vec::new(),
            preopens: Vec::new(),
            pool: Pool::new(),
            random: random::thread_rng(),
            insecure_random,
            insecure_random_seed,
            wall_clock: wall_clock(),
            monotonic_clock: monotonic_clock(),
            built: false,
        }
    }

    pub fn stdin(&mut self, stdin: impl HostInputStream + 'static, isatty: IsATTY) -> &mut Self {
        self.stdin = (Box::new(stdin), isatty);
        self
    }

    pub fn stdout(&mut self, stdout: impl HostOutputStream + 'static, isatty: IsATTY) -> &mut Self {
        self.stdout = (Box::new(stdout), isatty);
        self
    }

    pub fn stderr(&mut self, stderr: impl HostOutputStream + 'static, isatty: IsATTY) -> &mut Self {
        self.stderr = (Box::new(stderr), isatty);
        self
    }

    pub fn inherit_stdin(&mut self) -> &mut Self {
        use is_terminal::IsTerminal;
        let inherited = stdio::stdin();
        let isatty = if inherited.is_terminal() {
            IsATTY::Yes
        } else {
            IsATTY::No
        };
        self.stdin(inherited, isatty)
    }

    pub fn inherit_stdout(&mut self) -> &mut Self {
        use is_terminal::IsTerminal;
        let inherited = stdio::stdout();
        let isatty = if inherited.is_terminal() {
            IsATTY::Yes
        } else {
            IsATTY::No
        };
        self.stdout(inherited, isatty)
    }

    pub fn inherit_stderr(&mut self) -> &mut Self {
        use is_terminal::IsTerminal;
        let inherited = stdio::stderr();
        let isatty = if inherited.is_terminal() {
            IsATTY::Yes
        } else {
            IsATTY::No
        };
        self.stderr(inherited, isatty)
    }

    pub fn inherit_stdio(&mut self) -> &mut Self {
        self.inherit_stdin().inherit_stdout().inherit_stderr()
    }

    pub fn envs(&mut self, env: &[(impl AsRef<str>, impl AsRef<str>)]) -> &mut Self {
        self.env.extend(
            env.iter()
                .map(|(k, v)| (k.as_ref().to_owned(), v.as_ref().to_owned())),
        );
        self
    }

    pub fn env(&mut self, k: impl AsRef<str>, v: impl AsRef<str>) -> &mut Self {
        self.env
            .push((k.as_ref().to_owned(), v.as_ref().to_owned()));
        self
    }

    pub fn args(&mut self, args: &[impl AsRef<str>]) -> &mut Self {
        self.args.extend(args.iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn arg(&mut self, arg: impl AsRef<str>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn preopened_dir(
        &mut self,
        dir: cap_std::fs::Dir,
        perms: DirPerms,
        file_perms: FilePerms,
        path: impl AsRef<str>,
    ) -> &mut Self {
        self.preopens
            .push((Dir::new(dir, perms, file_perms), path.as_ref().to_owned()));
        self
    }

    /// Set the generator for the secure random number generator to the custom
    /// generator specified.
    ///
    /// Note that contexts have a default RNG configured which is a suitable
    /// generator for WASI and is configured with a random seed per-context.
    ///
    /// Guest code may rely on this random number generator to produce fresh
    /// unpredictable random data in order to maintain its security invariants,
    /// and ideally should use the insecure random API otherwise, so using any
    /// prerecorded or otherwise predictable data may compromise security.
    pub fn secure_random(&mut self, random: impl RngCore + Send + Sync + 'static) -> &mut Self {
        self.random = Box::new(random);
        self
    }

    pub fn insecure_random(
        &mut self,
        insecure_random: impl RngCore + Send + Sync + 'static,
    ) -> &mut Self {
        self.insecure_random = Box::new(insecure_random);
        self
    }
    pub fn insecure_random_seed(&mut self, insecure_random_seed: u128) -> &mut Self {
        self.insecure_random_seed = insecure_random_seed;
        self
    }

    pub fn wall_clock(&mut self, clock: impl clocks::HostWallClock + 'static) -> &mut Self {
        self.wall_clock = Box::new(clock);
        self
    }

    pub fn monotonic_clock(
        &mut self,
        clock: impl clocks::HostMonotonicClock + 'static,
    ) -> &mut Self {
        self.monotonic_clock = Box::new(clock);
        self
    }

    /// Add all network addresses accessable to the host to the pool.
    pub fn inherit_network(&mut self, ambient_authority: AmbientAuthority) -> &mut Self {
        self.pool.insert_ip_net_port_any(
            IpNet::new(Ipv4Addr::UNSPECIFIED.into(), 0).unwrap(),
            ambient_authority,
        );
        self.pool.insert_ip_net_port_any(
            IpNet::new(Ipv6Addr::UNSPECIFIED.into(), 0).unwrap(),
            ambient_authority,
        );
        self
    }

    /// Add network addresses to the pool.
    pub fn insert_addr<A: cap_std::net::ToSocketAddrs>(&mut self, addrs: A) -> std::io::Result<()> {
        self.pool.insert(addrs, ambient_authority())
    }

    /// Add a specific [`cap_std::net::SocketAddr`] to the pool.
    pub fn insert_socket_addr(&mut self, addr: cap_std::net::SocketAddr) {
        self.pool.insert_socket_addr(addr, ambient_authority());
    }

    /// Add a range of network addresses, accepting any port, to the pool.
    ///
    /// Unlike `insert_ip_net`, this function grants access to any requested port.
    pub fn insert_ip_net_port_any(&mut self, ip_net: ipnet::IpNet) {
        self.pool
            .insert_ip_net_port_any(ip_net, ambient_authority())
    }

    /// Add a range of network addresses, accepting a range of ports, to
    /// per-instance networks.
    ///
    /// This grants access to the port range starting at `ports_start` and, if
    /// `ports_end` is provided, ending before `ports_end`.
    pub fn insert_ip_net_port_range(
        &mut self,
        ip_net: ipnet::IpNet,
        ports_start: u16,
        ports_end: Option<u16>,
    ) {
        self.pool
            .insert_ip_net_port_range(ip_net, ports_start, ports_end, ambient_authority())
    }

    /// Add a range of network addresses with a specific port to the pool.
    pub fn insert_ip_net(&mut self, ip_net: ipnet::IpNet, port: u16) {
        self.pool.insert_ip_net(ip_net, port, ambient_authority())
    }

    /// Uses the configured context so far to construct the final `WasiCtx`.
    ///
    /// This will insert resources into the provided `table`.
    ///
    /// Note that each `WasiCtxBuilder` can only be used to "build" once, and
    /// calling this method twice will panic.
    ///
    /// # Panics
    ///
    /// Panics if this method is called twice.
    pub fn build(&mut self, table: &mut Table) -> Result<WasiCtx, anyhow::Error> {
        assert!(!self.built);

        use anyhow::Context;
        let Self {
            stdin,
            stdout,
            stderr,
            env,
            args,
            preopens,
            pool,
            random,
            insecure_random,
            insecure_random_seed,
            wall_clock,
            monotonic_clock,
            built: _,
        } = mem::replace(self, Self::new());
        self.built = true;

        let stdin_ix = table.push_input_stream(stdin.0).context("stdin")?;
        let stdout_ix = table.push_output_stream(stdout.0).context("stdout")?;
        let stderr_ix = table.push_output_stream(stderr.0).context("stderr")?;

        println!("stdin_ix: {}", stdin_ix);
        println!("stdout_ix: {}", stdout_ix);
        println!("stderr_ix: {}", stderr_ix);

        let preopens = preopens
            .into_iter()
            .map(|(dir, path)| {
                let dirfd = table
                    .push_dir(dir)
                    .with_context(|| format!("preopen {path:?}"))?;
                Ok((dirfd, path))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(WasiCtx {
            stdin: StdioInput {
                input_stream: stdin_ix,
                isatty: stdin.1,
            },
            stdout: StdioOutput {
                output_stream: stdout_ix,
                isatty: stdout.1,
            },
            stderr: StdioOutput {
                output_stream: stderr_ix,
                isatty: stderr.1,
            },
            env,
            args,
            preopens,
            pool,
            random,
            insecure_random,
            insecure_random_seed,
            wall_clock,
            monotonic_clock,
        })
    }
}

pub trait WasiView: Send {
    fn table(&self) -> &Table;
    fn table_mut(&mut self) -> &mut Table;
    fn ctx(&self) -> &WasiCtx;
    fn ctx_mut(&mut self) -> &mut WasiCtx;
}

pub struct WasiCtx {
    pub(crate) random: Box<dyn RngCore + Send + Sync>,
    pub(crate) insecure_random: Box<dyn RngCore + Send + Sync>,
    pub(crate) insecure_random_seed: u128,
    pub(crate) wall_clock: Box<dyn HostWallClock + Send + Sync>,
    pub(crate) monotonic_clock: Box<dyn HostMonotonicClock + Send + Sync>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) args: Vec<String>,
    pub(crate) preopens: Vec<(u32, String)>,
    pub(crate) stdin: StdioInput,
    pub(crate) stdout: StdioOutput,
    pub(crate) stderr: StdioOutput,
    pub(crate) pool: Pool,
}
