//! The Linux facilities the native stack accelerates with, read from netlink, sysfs and procfs.
//!
//! Nothing is parsed from a tool's output, and nothing that cannot be read is guessed: a facility
//! the machine does not answer for is a [`Probe`] error carrying the reason.

use std::{collections::HashMap, fmt, fs, io, path::Path, str::FromStr};

use ethtool::{
    EthtoolAttr, EthtoolBitset, EthtoolBitsetValue, EthtoolError, EthtoolFeatureAttr,
    EthtoolHandle, EthtoolHeader, EthtoolRingAttr,
};
use futures_util::StreamExt as _;
use skyzen_core::steering::{physical_nics, Placement, Unavailable};

use crate::output;

/// Why a facility's state could not be determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unknown(pub String);

impl fmt::Display for Unknown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A facility's state, or why it is unknown.
pub type Probe<T> = Result<T, Unknown>;

/// Everything the native stack can use on this machine, and its current state.
#[derive(Debug)]
pub struct Host {
    /// The running kernel's release (`/proc/sys/kernel/osrelease`).
    pub kernel: Probe<String>,
    /// The kernel `tls` module, which kernel TLS and NIC TLS offload need.
    pub kernel_tls: Probe<KernelTls>,
    /// Each physical network interface.
    pub nics: Probe<Vec<Nic>>,
    /// The machine-wide busy-polling settings.
    pub busy_poll: BusyPoll,
    /// What decides whether the runtime steers connections here.
    pub steering: Probe<Steering>,
    /// What bounds the memory zero-copy sends pin.
    pub locked_memory: LockedMemory,
}

/// What decides whether a zero-copy send happens under load.
///
/// A zero-copy send pins the pages it sends until the peer acknowledges them, and `io_uring`
/// charges them to the user's `RLIMIT_MEMLOCK` unless the process has `CAP_IPC_LOCK`
/// (`io_notif_account_mem`); a send past the limit is refused by the kernel and copies instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedMemory {
    /// This process's `RLIMIT_MEMLOCK`, from `getrlimit(2)`.
    pub limit: Probe<MemlockLimit>,
    /// Whether this process has `CAP_IPC_LOCK` in its effective set (`CapEff` in
    /// `/proc/self/status`).
    pub ipc_lock: Probe<bool>,
}

/// An `RLIMIT_MEMLOCK` pair, in bytes; `None` is unlimited (`RLIM_INFINITY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemlockLimit {
    /// The soft limit, the one the kernel charges against.
    pub soft: Option<u64>,
    /// The hard limit, the most the soft one can be raised to without privilege.
    pub hard: Option<u64>,
}

/// `CAP_IPC_LOCK`'s bit in a capability set (`include/uapi/linux/capability.h`).
const CAP_IPC_LOCK: u32 = 14;

/// The kernel `tls` module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelTls {
    /// Registered with TCP (`tls` is in `net.ipv4.tcp_available_ulp`), loaded or built in.
    Loaded,
    /// Not loaded, but the running kernel ships it as a module.
    Loadable,
    /// The running kernel has no `tls` module.
    Absent,
}

/// A NIC offload feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offload {
    /// The feature is on.
    Active,
    /// The device can turn it on, and it is off.
    Inactive,
    /// The device cannot turn it on.
    Unsupported,
}

/// Header/data split, which zero-copy receive needs: the NIC writes headers and payload to
/// separate buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSplit {
    /// The NIC splits headers from data.
    Enabled,
    /// The NIC does not.
    Disabled,
}

/// One physical network interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nic {
    /// The interface name.
    pub name: String,
    /// `tls-hw-tx-offload`: the NIC encrypts kernel-TLS records it sends.
    pub tls_tx: Probe<Offload>,
    /// `tls-hw-rx-offload`: the NIC decrypts kernel-TLS records it receives.
    pub tls_rx: Probe<Offload>,
    /// `tcp-data-split` from the ring parameters.
    pub data_split: Probe<DataSplit>,
    /// `napi_defer_hard_irqs`: how many empty polls defer re-enabling the queue's interrupt.
    pub napi_defer_hard_irqs: Probe<u32>,
    /// `gro_flush_timeout`, in nanoseconds: how long a deferred interrupt waits.
    pub gro_flush_timeout: Probe<u64>,
}

/// The machine-wide busy-polling settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyPoll {
    /// `net.core.busy_poll`, in microseconds; 0 is off.
    pub poll_micros: Probe<u32>,
    /// `net.core.busy_read`, in microseconds; 0 is off.
    pub read_micros: Probe<u32>,
    /// Whether an `io_uring` can register for NAPI busy polling (`IORING_REGISTER_NAPI`).
    pub napi_registration: Probe<bool>,
}

/// What the runtime's automatic steering decides on this machine.
#[derive(Debug)]
pub struct Steering {
    /// How many serving threads a start would run: this process's available parallelism.
    pub threads: usize,
    /// Where those threads would be pinned, and the NIC queue layout they are judged against.
    pub placement: Result<Placement, Unavailable>,
}

impl Host {
    /// Read every facility's support and state.
    pub async fn probe() -> Self {
        let kernel = read_text(Path::new("/proc/sys/kernel/osrelease"));
        Self {
            kernel_tls: kernel_tls(&kernel),
            nics: nics().await,
            busy_poll: BusyPoll::probe(),
            steering: steering(),
            locked_memory: LockedMemory::probe(),
            kernel,
        }
    }

    /// Print the report `skyzen doctor` shows.
    pub fn report(&self) {
        match &self.kernel {
            Ok(release) => output::ok(format!("native: Linux {release}")),
            Err(why) => output::unknown(format!("native: kernel release: {why}")),
        }
        match &self.kernel_tls {
            Ok(KernelTls::Loaded) => output::ok("native: kernel TLS: the `tls` module is loaded"),
            Ok(KernelTls::Loadable) => output::off(
                "native: kernel TLS: the kernel ships the `tls` module, and it is not loaded",
            ),
            Ok(KernelTls::Absent) => {
                output::off("native: kernel TLS: this kernel has no `tls` module");
            }
            Err(why) => output::unknown(format!("native: kernel TLS: {why}")),
        }
        match &self.nics {
            Ok(nics) if nics.is_empty() => output::off(
                "native: no physical network interface, so no NIC offload, header/data split or \
                 receive queue",
            ),
            Ok(nics) => nics.iter().for_each(Nic::report),
            Err(why) => output::unknown(format!("native: network interfaces: {why}")),
        }
        self.busy_poll.report();
        match &self.steering {
            Ok(steering) => steering.report(),
            Err(why) => output::unknown(format!("native: connection steering: {why}")),
        }
        self.locked_memory.report();
    }
}

impl LockedMemory {
    fn probe() -> Self {
        use nix::sys::resource::{getrlimit, Resource, RLIM_INFINITY};

        let finite = |limit| (limit != RLIM_INFINITY).then_some(limit);
        Self {
            limit: getrlimit(Resource::RLIMIT_MEMLOCK)
                .map(|(soft, hard)| MemlockLimit {
                    soft: finite(soft),
                    hard: finite(hard),
                })
                .map_err(|error| Unknown(format!("getrlimit(RLIMIT_MEMLOCK) failed: {error}"))),
            ipc_lock: read_text(Path::new("/proc/self/status"))
                .and_then(|status| effective_capability(&status, CAP_IPC_LOCK)),
        }
    }

    fn report(&self) {
        const WHY: &str = "the pages a zero-copy send pins until the peer acknowledges them";
        let limit = self
            .limit
            .as_ref()
            .map(|limit| {
                let show = |bytes: Option<u64>| bytes.map_or_else(|| "unlimited".to_owned(), size);
                (
                    limit.soft,
                    format!("{} (hard {})", show(limit.soft), show(limit.hard)),
                )
            })
            .map_err(Clone::clone);
        match (&self.ipc_lock, &limit) {
            (Ok(true), _) => output::ok(format!(
                "native: zero-copy sends: this process has CAP_IPC_LOCK, so {WHY} are not \
                 charged to RLIMIT_MEMLOCK"
            )),
            (Ok(false), Ok((None, shown))) => output::ok(format!(
                "native: zero-copy sends: RLIMIT_MEMLOCK is {shown}, so {WHY} never reach it"
            )),
            (Ok(false), Ok((Some(_), shown))) => output::warn(format!(
                "native: zero-copy sends: RLIMIT_MEMLOCK is {shown} and this process lacks \
                 CAP_IPC_LOCK, so {WHY} count against it, and under load a send past it copies \
                 instead; raise the limit (`LimitMEMLOCK=` in a systemd unit, `ulimit -l`) or \
                 grant CAP_IPC_LOCK"
            )),
            (Err(why), _) | (_, Err(why)) => {
                output::unknown(format!("native: zero-copy sends: {why}"));
            }
        }
    }
}

/// Whether capability `cap` is in the effective set a `/proc/<pid>/status` text reports.
fn effective_capability(status: &str, cap: u32) -> Probe<bool> {
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .ok_or_else(|| Unknown("/proc/self/status has no CapEff line".to_owned()))?;
    let set = u64::from_str_radix(line.trim(), 16).map_err(|error| {
        Unknown(format!(
            "/proc/self/status's CapEff `{}` is not a hex set: {error}",
            line.trim()
        ))
    })?;
    Ok(set >> cap & 1 == 1)
}

/// `bytes` in the largest binary unit that divides it.
fn size(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    const KIB: u64 = 1 << 10;
    if bytes != 0 && bytes % MIB == 0 {
        format!("{} MiB", bytes / MIB)
    } else if bytes != 0 && bytes % KIB == 0 {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

impl Nic {
    fn report(&self) {
        let name = &self.name;
        for (feature, state) in [
            ("tls-hw-tx-offload", &self.tls_tx),
            ("tls-hw-rx-offload", &self.tls_rx),
        ] {
            match state {
                Ok(Offload::Active) => output::ok(format!("native: {name}: {feature} is on")),
                Ok(Offload::Inactive) => {
                    output::off(format!("native: {name}: {feature} is supported and off"));
                }
                Ok(Offload::Unsupported) => {
                    output::off(format!("native: {name}: {feature} is not supported"));
                }
                Err(why) => output::unknown(format!("native: {name}: {feature}: {why}")),
            }
        }
        match &self.data_split {
            Ok(DataSplit::Enabled) => {
                output::ok(format!("native: {name}: header/data split is on"));
            }
            Ok(DataSplit::Disabled) => {
                output::off(format!("native: {name}: header/data split is off"));
            }
            Err(why) => output::unknown(format!("native: {name}: header/data split: {why}")),
        }
        match (&self.napi_defer_hard_irqs, &self.gro_flush_timeout) {
            (Ok(0), Ok(timeout)) => output::off(format!(
                "native: {name}: interrupt deferral is off (napi_defer_hard_irqs 0, \
                 gro_flush_timeout {timeout} ns)"
            )),
            (Ok(defer), Ok(timeout)) => output::ok(format!(
                "native: {name}: interrupt deferral is on (napi_defer_hard_irqs {defer}, \
                 gro_flush_timeout {timeout} ns)"
            )),
            (Err(why), _) | (_, Err(why)) => {
                output::unknown(format!("native: {name}: interrupt deferral: {why}"));
            }
        }
    }
}

impl BusyPoll {
    fn probe() -> Self {
        Self {
            poll_micros: read_number(Path::new("/proc/sys/net/core/busy_poll")),
            read_micros: read_number(Path::new("/proc/sys/net/core/busy_read")),
            napi_registration: napi_registration(),
        }
    }

    fn report(&self) {
        for (sysctl, value) in [
            ("net.core.busy_poll", &self.poll_micros),
            ("net.core.busy_read", &self.read_micros),
        ] {
            match value {
                Ok(0) => output::off(format!("native: busy polling: {sysctl} is 0 (off)")),
                Ok(micros) => output::ok(format!("native: busy polling: {sysctl} is {micros} µs")),
                Err(why) => output::unknown(format!("native: busy polling: {sysctl}: {why}")),
            }
        }
        match &self.napi_registration {
            Ok(true) => output::ok("native: busy polling: io_uring NAPI registration is supported"),
            Ok(false) => output::off(
                "native: busy polling: this kernel has no io_uring NAPI registration (Linux 6.9)",
            ),
            Err(why) => {
                output::unknown(format!(
                    "native: busy polling: io_uring NAPI registration: {why}"
                ));
            }
        }
    }
}

impl Steering {
    fn report(&self) {
        let threads = self.threads;
        let placement = match &self.placement {
            Ok(placement) => placement,
            Err(error @ Unavailable::Affinity(_)) => {
                output::warn(format!(
                    "native: connection steering: {error}; a server with {threads} threads would \
                     refuse to start unless steering is off"
                ));
                return;
            }
            Err(error @ Unavailable::Layout(_)) => {
                output::off(format!(
                    "native: connection steering stays off on `auto`: {error}"
                ));
                return;
            }
        };
        let mut by_nic: Vec<(&str, Vec<String>)> = Vec::new();
        for queue in &placement.layout.queues {
            let entry = format!("IRQ {} -> CPU {}", queue.irq, queue.cpu);
            match by_nic.iter_mut().find(|(nic, _)| *nic == queue.nic) {
                Some((_, entries)) => entries.push(entry),
                None => by_nic.push((&queue.nic, vec![entry])),
            }
        }
        if by_nic.is_empty() {
            output::off("native: no NIC receive-queue interrupt is delivered to a single CPU");
        }
        for (nic, entries) in by_nic {
            output::ok(format!(
                "native: {nic}: receive-queue interrupts {}",
                entries.join(", ")
            ));
        }
        let cpus = &placement.cpus;
        if placement.steers() {
            output::ok(format!(
                "native: connection steering turns on on `auto`: each of the {threads} serving \
                 CPUs {cpus:?} takes a NIC receive queue's interrupt"
            ));
        } else {
            output::off(format!(
                "native: connection steering stays off on `auto`: of the {threads} serving CPUs \
                 {cpus:?}, {:?} take no NIC receive queue's interrupt; point the queue \
                 interrupts at every serving CPU, since forcing it on here refuses to start",
                placement.unserved()
            ));
        }
    }
}

/// Read a procfs or sysfs file, trimmed.
fn read_text(path: &Path) -> Probe<String> {
    fs::read_to_string(path)
        .map(|text| text.trim().to_owned())
        .map_err(|error| Unknown(format!("cannot read {}: {error}", path.display())))
}

/// Read a procfs or sysfs file holding one number.
fn read_number<T: FromStr>(path: &Path) -> Probe<T>
where
    T::Err: fmt::Display,
{
    let text = read_text(path)?;
    text.parse().map_err(|error| {
        Unknown(format!(
            "{} holds `{text}`, not a number: {error}",
            path.display()
        ))
    })
}

/// Whether TCP has the `tls` upper-layer protocol, and else whether the kernel ships the module.
fn kernel_tls(release: &Probe<String>) -> Probe<KernelTls> {
    let ulps = read_text(Path::new("/proc/sys/net/ipv4/tcp_available_ulp"))?;
    if ulps.split_whitespace().any(|ulp| ulp == "tls") {
        return Ok(KernelTls::Loaded);
    }
    // No `/proc/modules` is a kernel built without loadable modules: nothing can add `tls`.
    if !Path::new("/proc/modules").exists() {
        return Ok(KernelTls::Absent);
    }
    let release = release.as_ref().map_err(Clone::clone)?;
    let modules = read_text(&Path::new("/lib/modules").join(release).join("modules.dep"))?;
    Ok(if modules.lines().any(is_tls_module) {
        KernelTls::Loadable
    } else {
        KernelTls::Absent
    })
}

/// Whether a `modules.dep` line is `net/tls/tls.ko`, compressed or not.
fn is_tls_module(line: &str) -> bool {
    let path = line.split(':').next().unwrap_or_default();
    path.rsplit_once('/').is_some_and(|(dir, file)| {
        dir.ends_with("net/tls") && (file == "tls.ko" || file.starts_with("tls.ko."))
    })
}

/// Probe whether an `io_uring` accepts a NAPI registration, by making one.
fn napi_registration() -> Probe<bool> {
    let ring = io_uring::IoUring::new(1)
        .map_err(|error| Unknown(format!("an io_uring cannot be set up here: {error}")))?;
    match ring
        .submitter()
        .register_napi(&mut io_uring::types::Napi::new())
    {
        Ok(()) => Ok(true),
        // An opcode the kernel does not know is EINVAL.
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(false),
        Err(error) => Err(Unknown(format!("registering NAPI failed: {error}"))),
    }
}

/// The steering decision for as many threads as a start here would run.
fn steering() -> Probe<Steering> {
    let threads = std::thread::available_parallelism()
        .map_err(|error| Unknown(format!("the available parallelism cannot be read: {error}")))?
        .get();
    Ok(Steering {
        threads,
        placement: Placement::read(threads),
    })
}

/// Each physical interface, its offloads from ethtool netlink and its interrupt deferral from
/// sysfs.
async fn nics() -> Probe<Vec<Nic>> {
    let names =
        physical_nics().map_err(|error| Unknown(format!("cannot list /sys/class/net: {error}")))?;
    let ethtool = ethtool().await;
    Ok(names
        .into_iter()
        .map(|name| {
            let sys = Path::new("/sys/class/net").join(&name);
            let (tls_tx, tls_rx, data_split) = match &ethtool {
                Ok(ethtool) => ethtool.of(&name),
                Err(why) => (Err(why.clone()), Err(why.clone()), Err(why.clone())),
            };
            Nic {
                tls_tx,
                tls_rx,
                data_split,
                napi_defer_hard_irqs: read_number(&sys.join("napi_defer_hard_irqs")),
                gro_flush_timeout: read_number(&sys.join("gro_flush_timeout")),
                name,
            }
        })
        .collect())
}

/// The ethtool netlink replies for every interface, by name.
#[derive(Debug)]
struct Ethtool {
    /// `tls-hw-tx-offload` and `tls-hw-rx-offload`.
    tls: HashMap<String, [Probe<Offload>; 2]>,
    /// The `tcp-data-split` ring parameter, when the driver reports one.
    data_split: HashMap<String, Option<u8>>,
}

impl Ethtool {
    fn of(&self, nic: &str) -> (Probe<Offload>, Probe<Offload>, Probe<DataSplit>) {
        let [tx, rx] = self.tls.get(nic).cloned().unwrap_or_else(|| {
            let why = Unknown("the kernel reported no features for it".to_owned());
            [Err(why.clone()), Err(why)]
        });
        // `ETHTOOL_TCP_DATA_SPLIT_DISABLED` and `_ENABLED` in `include/uapi/linux/ethtool.h`;
        // the kernel leaves the attribute out when the driver reports `_UNKNOWN`.
        let data_split = match self.data_split.get(nic) {
            None => Err(Unknown("the driver reports no ring parameters".to_owned())),
            Some(None) => Err(Unknown(
                "the driver does not report it (or the kernel predates 6.0)".to_owned(),
            )),
            Some(Some(1)) => Ok(DataSplit::Disabled),
            Some(Some(2)) => Ok(DataSplit::Enabled),
            Some(Some(other)) => Err(Unknown(format!("the kernel reported state {other}"))),
        };
        (tx, rx, data_split)
    }
}

/// Dump the features and ring parameters of every interface over ethtool netlink.
async fn ethtool() -> Probe<Ethtool> {
    let (connection, handle, _) = ethtool::new_connection()
        .map_err(|error| Unknown(format!("cannot open a generic netlink socket: {error}")))?;
    let driver = tokio::spawn(connection);
    let replies =
        futures_util::future::try_join(tls_features(handle.clone()), data_split(handle)).await;
    driver.abort();
    let (tls, data_split) = replies?;
    Ok(Ethtool { tls, data_split })
}

impl From<EthtoolError> for Unknown {
    fn from(error: EthtoolError) -> Self {
        Self(format!("ethtool netlink: {error}"))
    }
}

/// The interface an ethtool reply's header names.
fn device_name(header: Vec<EthtoolHeader>) -> Option<String> {
    header.into_iter().find_map(|attr| match attr {
        EthtoolHeader::DevName(name) => Some(name),
        _ => None,
    })
}

async fn tls_features(mut handle: EthtoolHandle) -> Probe<HashMap<String, [Probe<Offload>; 2]>> {
    let mut replies = std::pin::pin!(handle
        .feature()
        .get(None)
        .execute()
        .await
        .map_err(Unknown::from)?);
    let mut nics = HashMap::new();
    while let Some(reply) = replies.next().await {
        let (mut name, mut hw, mut active) = (None, None, None);
        for attr in reply?.payload.nlas {
            match attr {
                EthtoolAttr::Feature(EthtoolFeatureAttr::Header(header)) => {
                    name = device_name(header);
                }
                EthtoolAttr::Feature(EthtoolFeatureAttr::Hw(set)) => hw = Some(set),
                EthtoolAttr::Feature(EthtoolFeatureAttr::Active(set)) => active = Some(set),
                _ => {}
            }
        }
        if let Some(name) = name {
            let state = |feature| offload(hw.as_ref(), active.as_ref(), feature);
            nics.insert(
                name,
                [state("tls-hw-tx-offload"), state("tls-hw-rx-offload")],
            );
        }
    }
    Ok(nics)
}

async fn data_split(mut handle: EthtoolHandle) -> Probe<HashMap<String, Option<u8>>> {
    let mut replies = std::pin::pin!(handle
        .ring()
        .get(None)
        .execute()
        .await
        .map_err(Unknown::from)?);
    let mut nics = HashMap::new();
    while let Some(reply) = replies.next().await {
        let (mut name, mut split) = (None, None);
        for attr in reply?.payload.nlas {
            match attr {
                EthtoolAttr::Ring(EthtoolRingAttr::Header(header)) => name = device_name(header),
                EthtoolAttr::Ring(EthtoolRingAttr::TcpDataSplit(state)) => split = Some(state),
                _ => {}
            }
        }
        if let Some(name) = name {
            nics.insert(name, split);
        }
    }
    Ok(nics)
}

/// A feature's state from the device's changeable (`hw`) and active sets.
fn offload(
    hw: Option<&EthtoolBitset>,
    active: Option<&EthtoolBitset>,
    feature: &str,
) -> Probe<Offload> {
    let (Some(hw), Some(active)) = (hw, active) else {
        return Err(Unknown(
            "the kernel's feature reply has no changeable or active set".to_owned(),
        ));
    };
    match (is_set(active, feature), is_set(hw, feature)) {
        (Some(true), _) => Ok(Offload::Active),
        (Some(false), Some(true)) => Ok(Offload::Inactive),
        (Some(false), Some(false)) => Ok(Offload::Unsupported),
        _ => Err(Unknown(
            "the kernel answered with compact bitsets, which carry no feature names".to_owned(),
        )),
    }
}

/// Whether a verbose bitset has the named bit set; a set without a mask lists only its set bits.
fn is_set(set: &EthtoolBitset, name: &str) -> Option<bool> {
    match &set.value {
        EthtoolBitsetValue::Bits(bits) => Some(
            bits.iter()
                .any(|bit| bit.name == name && (set.nomask || bit.value)),
        ),
        EthtoolBitsetValue::Compact { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use ethtool::EthtoolBit;

    use super::*;

    /// `/proc/self/status` of a process whose effective set is `CAP_NET_BIND_SERVICE` (10) and
    /// `CAP_IPC_LOCK` (14), and of one with none.
    #[test]
    fn cap_ipc_lock_is_read_from_the_effective_set() {
        let holding = "Name:\tskyzen\nCapInh:\t0000000000000000\nCapPrm:\t0000000000004400\n\
                       CapEff:\t0000000000004400\nCapBnd:\t000001ffffffffff\n";
        assert_eq!(effective_capability(holding, CAP_IPC_LOCK), Ok(true));
        let lacking = "CapPrm:\t0000000000004000\nCapEff:\t0000000000000400\n";
        assert_eq!(effective_capability(lacking, CAP_IPC_LOCK), Ok(false));
        assert!(effective_capability("Name:\tskyzen\n", CAP_IPC_LOCK).is_err());
        assert!(effective_capability("CapEff:\tzz\n", CAP_IPC_LOCK).is_err());
    }

    #[test]
    fn the_locked_memory_limit_is_shown_in_its_largest_unit() {
        // Linux's default `MLOCK_LIMIT` since 5.16: 8 MiB.
        assert_eq!(size(8 << 20), "8 MiB");
        assert_eq!(size(64 << 10), "64 KiB");
        assert_eq!(size(1000), "1000 bytes");
        assert_eq!(size(0), "0 bytes");
    }

    /// The probe answers for this process: both halves are readable on any Linux.
    #[test]
    fn the_locked_memory_probe_reads_this_process() {
        let probed = LockedMemory::probe();
        assert!(probed.limit.is_ok(), "{probed:?}");
        assert!(probed.ipc_lock.is_ok(), "{probed:?}");
    }

    #[test]
    fn the_tls_module_is_found_in_modules_dep() {
        assert!(is_tls_module("kernel/net/tls/tls.ko.zst:"));
        assert!(is_tls_module("kernel/net/tls/tls.ko: kernel/lib/x.ko"));
        assert!(!is_tls_module("kernel/drivers/net/tls_other.ko:"));
        assert!(!is_tls_module("kernel/net/tls/tlsx.ko:"));
    }

    fn bits(nomask: bool, set: &[(&str, bool)]) -> EthtoolBitset {
        EthtoolBitset {
            nomask,
            size: 64,
            value: EthtoolBitsetValue::Bits(
                set.iter()
                    .enumerate()
                    .map(|(index, (name, value))| EthtoolBit {
                        index: u32::try_from(index).unwrap(),
                        name: (*name).to_owned(),
                        value: *value,
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn an_offload_is_active_changeable_or_unsupported() {
        let feature = "tls-hw-tx-offload";
        let hw = bits(true, &[(feature, false)]);
        let none = bits(true, &[]);
        assert_eq!(offload(Some(&hw), Some(&hw), feature), Ok(Offload::Active));
        assert_eq!(
            offload(Some(&hw), Some(&none), feature),
            Ok(Offload::Inactive)
        );
        assert_eq!(
            offload(Some(&none), Some(&none), feature),
            Ok(Offload::Unsupported)
        );
        let masked = bits(false, &[(feature, false)]);
        assert_eq!(
            offload(Some(&masked), Some(&masked), feature),
            Ok(Offload::Unsupported),
            "with a mask, a listed bit counts only when its value is set"
        );
        assert!(offload(None, Some(&hw), feature).is_err());
    }
}
