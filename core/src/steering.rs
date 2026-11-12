//! What decides whether the native runtime steers connections on this machine (Linux).
//!
//! Steering sends each connection to the serving thread pinned to the CPU whose NIC receive queue
//! took its packets, and it pays only when every serving CPU takes such a queue's interrupt. The
//! runtime turns it on automatically exactly then, and `skyzen doctor` explains the same decision
//! to an operator; both read the machine through [`Placement`], so they cannot disagree about it.
//!
//! Everything here is read from sysfs, procfs and the process's CPU affinity mask.

use std::{
    collections::HashMap,
    fmt, io,
    path::{Path, PathBuf},
};

use nix::{
    sched::{sched_getaffinity, CpuSet},
    unistd::Pid,
};

/// One NIC receive queue and the CPU its interrupt is delivered to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxQueue {
    /// The network interface.
    pub nic: String,
    /// The interrupt number.
    pub irq: u32,
    /// The one CPU that takes it.
    pub cpu: usize,
}

/// Which CPUs take a NIC receive queue's interrupt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RxLayout {
    /// Every receive queue whose interrupt is delivered to exactly one CPU.
    pub queues: Vec<RxQueue>,
}

impl RxLayout {
    /// The CPUs in `cpus` that take no receive queue's interrupt: steering to a thread there
    /// would move a connection to a core its packets never touched.
    #[must_use]
    pub fn unserved(&self, cpus: &[usize]) -> Vec<usize> {
        cpus.iter()
            .copied()
            .filter(|cpu| self.queues.iter().all(|queue| queue.cpu != *cpu))
            .collect()
    }
}

/// The serving CPUs a start would pin its threads to, and the receive-queue layout they are
/// judged against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Serving thread `i` takes the `i`-th CPU of the process's affinity mask.
    pub cpus: Vec<usize>,
    /// The machine's NIC receive queues.
    pub layout: RxLayout,
}

impl Placement {
    /// Read the placement `threads` serving threads would have on this machine.
    ///
    /// # Errors
    ///
    /// The affinity mask cannot be read or holds fewer than `threads` CPUs, or the
    /// receive-queue layout cannot be read.
    pub fn read(threads: usize) -> Result<Self, Unavailable> {
        let cpus = serving_cpus(threads).map_err(Unavailable::Affinity)?;
        let layout = rx_layout().map_err(Unavailable::Layout)?;
        Ok(Self { cpus, layout })
    }

    /// The serving CPUs that take no receive queue's interrupt.
    #[must_use]
    pub fn unserved(&self) -> Vec<usize> {
        self.layout.unserved(&self.cpus)
    }

    /// Whether automatic steering turns on: every serving CPU takes a receive queue's interrupt.
    #[must_use]
    pub fn steers(&self) -> bool {
        self.unserved().is_empty()
    }
}

/// Why the placement cannot be read.
#[derive(Debug)]
pub enum Unavailable {
    /// The process's CPU affinity cannot be read, or has fewer CPUs than serving threads.
    Affinity(io::Error),
    /// The NIC receive-queue layout cannot be read.
    Layout(io::Error),
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Affinity(error) => write!(f, "the CPU affinity mask cannot be read: {error}"),
            Self::Layout(error) => write!(f, "the NIC queue layout cannot be read: {error}"),
        }
    }
}

impl std::error::Error for Unavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Affinity(error) | Self::Layout(error) => Some(error),
        }
    }
}

/// The CPUs this process may run on, in ascending order.
///
/// # Errors
///
/// The kernel refuses `sched_getaffinity`.
pub fn affinity_mask() -> io::Result<Vec<usize>> {
    let set = sched_getaffinity(Pid::from_raw(0)).map_err(io::Error::from)?;
    Ok((0..CpuSet::count())
        .filter(|&cpu| set.is_set(cpu).unwrap_or(false))
        .collect())
}

/// The first `threads` CPUs of the affinity mask: the CPU each serving thread is pinned to.
///
/// # Errors
///
/// The mask cannot be read or holds fewer than `threads` CPUs.
pub fn serving_cpus(threads: usize) -> io::Result<Vec<usize>> {
    let mut cpus = affinity_mask()?;
    if cpus.len() < threads {
        return Err(io::Error::other(format!(
            "{threads} serving threads but the process may run on {} CPUs",
            cpus.len()
        )));
    }
    cpus.truncate(threads);
    Ok(cpus)
}

/// The machine's physical network interfaces, by name, in order.
///
/// # Errors
///
/// `/sys/class/net` cannot be read.
pub fn physical_nics() -> io::Result<Vec<String>> {
    physical_nics_at(Path::new("/sys"))
}

/// [`physical_nics`] over a sysfs mounted at `sys`.
///
/// A physical interface is one with a device behind it; loopback, bridges and veths have none.
///
/// # Errors
///
/// `class/net` under `sys` cannot be read.
pub fn physical_nics_at(sys: &Path) -> io::Result<Vec<String>> {
    let class = sys.join("class/net");
    let mut nics = Vec::new();
    for entry in std::fs::read_dir(&class)? {
        let entry = entry?;
        if entry.path().join("device").exists() {
            nics.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    nics.sort();
    Ok(nics)
}

/// Read the receive-queue layout of this machine's NICs.
///
/// A queue is a physical interface's interrupt whose action name marks it as receiving
/// (`rx`, `input`, `comp`, `ntfy`), and its CPU is the one its effective affinity names; an
/// interrupt spread over several CPUs has no single CPU and is left out.
///
/// # Errors
///
/// `/sys/class/net` or `/proc/interrupts` cannot be read.
pub fn rx_layout() -> io::Result<RxLayout> {
    rx_layout_at(Path::new("/sys"), Path::new("/proc"))
}

/// [`rx_layout`] over a sysfs and a procfs mounted at `sys` and `proc`.
///
/// # Errors
///
/// `class/net` under `sys` or `interrupts` under `proc` cannot be read.
pub fn rx_layout_at(sys: &Path, proc: &Path) -> io::Result<RxLayout> {
    let mut queues = Vec::new();
    for (nic, irqs) in rx_interrupts_at(sys, proc)? {
        for irq in irqs {
            if let Some(cpu) = irq_cpu(proc, irq) {
                queues.push(RxQueue {
                    nic: nic.clone(),
                    irq,
                    cpu,
                });
            }
        }
    }
    Ok(RxLayout { queues })
}

/// Each physical interface's receive-queue interrupts, wherever they are delivered, in ascending
/// order: the interrupts [`rx_layout`] judges and `skyzen deploy` places.
///
/// # Errors
///
/// `/sys/class/net` or `/proc/interrupts` cannot be read.
pub fn rx_interrupts() -> io::Result<Vec<(String, Vec<u32>)>> {
    rx_interrupts_at(Path::new("/sys"), Path::new("/proc"))
}

/// [`rx_interrupts`] over a sysfs and a procfs mounted at `sys` and `proc`.
///
/// # Errors
///
/// `class/net` under `sys` or `interrupts` under `proc` cannot be read.
pub fn rx_interrupts_at(sys: &Path, proc: &Path) -> io::Result<Vec<(String, Vec<u32>)>> {
    let names = interrupt_names(&proc.join("interrupts"))?;
    let mut nics = Vec::new();
    for nic in physical_nics_at(sys)? {
        let device = sys.join("class/net").join(&nic).join("device");
        let mut irqs: Vec<u32> = device_irqs(&device)?
            .into_iter()
            .filter(|irq| names.get(irq).is_some_and(|name| is_receive_name(name)))
            .collect();
        irqs.sort_unstable();
        nics.push((nic, irqs));
    }
    Ok(nics)
}

/// The CPUs interrupt `irq` may be delivered to, as an operator sets them
/// (`/proc/irq/<irq>/smp_affinity_list`).
///
/// # Errors
///
/// The file cannot be read or does not hold a CPU list.
pub fn irq_affinity(irq: u32) -> io::Result<Vec<usize>> {
    let path = Path::new("/proc/irq")
        .join(irq.to_string())
        .join(SMP_AFFINITY_LIST);
    let list = std::fs::read_to_string(&path)?;
    parse_cpu_list(list.trim()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} holds `{}`, not a CPU list", path.display(), list.trim()),
        )
    })
}

/// The file under `/proc/irq/<irq>/` that holds, and sets, where an interrupt may be delivered.
pub const SMP_AFFINITY_LIST: &str = "smp_affinity_list";

/// The action name of each interrupt in `/proc/interrupts`, by number.
fn interrupt_names(path: &Path) -> io::Result<HashMap<u32, String>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let (irq, rest) = line.split_once(':')?;
            let irq = irq.trim().parse().ok()?;
            Some((irq, rest.split_whitespace().last()?.to_owned()))
        })
        .collect())
}

/// The interrupts a network device owns: its MSI vectors, which sysfs lists on the PCI device
/// (a virtio device is a child of it), or else its one legacy line.
fn device_irqs(device: &Path) -> io::Result<Vec<u32>> {
    let mut dir = std::fs::canonicalize(device)?;
    for _ in 0..3 {
        let msi: PathBuf = dir.join("msi_irqs");
        if msi.is_dir() {
            return Ok(std::fs::read_dir(msi)?
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect());
        }
        if !dir.pop() {
            break;
        }
    }
    Ok(std::fs::read_to_string(device.join("irq"))
        .ok()
        .and_then(|irq| irq.trim().parse().ok())
        .into_iter()
        .collect())
}

/// Whether a NIC interrupt's action name is a queue that receives.
fn is_receive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["rx", "input", "comp", "ntfy"]
        .iter()
        .any(|marker| name.contains(marker))
}

/// The one CPU interrupt `irq` is delivered to, if it is exactly one.
fn irq_cpu(proc: &Path, irq: u32) -> Option<usize> {
    let dir = proc.join("irq").join(irq.to_string());
    let list = std::fs::read_to_string(dir.join("effective_affinity_list"))
        .or_else(|_| std::fs::read_to_string(dir.join(SMP_AFFINITY_LIST)))
        .ok()?;
    let cpus = parse_cpu_list(list.trim())?;
    match cpus.as_slice() {
        [cpu] => Some(*cpu),
        _ => None,
    }
}

/// Parse a kernel CPU list such as `0-3,8`.
fn parse_cpu_list(list: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in list.split(',').filter(|part| !part.is_empty()) {
        match part.split_once('-') {
            Some((low, high)) => cpus.extend(low.parse::<usize>().ok()?..=high.parse().ok()?),
            None => cpus.push(part.parse().ok()?),
        }
    }
    Some(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_parse() {
        assert_eq!(parse_cpu_list("0-2,8"), Some(vec![0, 1, 2, 8]));
        assert_eq!(parse_cpu_list("5"), Some(vec![5]));
        assert_eq!(parse_cpu_list("x"), None);
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_layout_lists_the_cpus_of_receive_queue_interrupts() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write(
            root,
            "proc/interrupts",
            "           CPU0       CPU1\n  24:  1  0  PCI-MSIX  eth0-TxRx-0\n  25:  0  1  PCI-MSIX  eth0-TxRx-1\n  26:  0  0  PCI-MSIX  eth0-config\n  27:  0  0  PCI-MSIX  eth0-TxRx-2\n",
        );
        for (irq, cpus) in [(24, "0"), (25, "1"), (26, "0"), (27, "0-1")] {
            write(
                root,
                &format!("proc/irq/{irq}/effective_affinity_list"),
                cpus,
            );
            write(root, &format!("sys/devices/pci0/msi_irqs/{irq}"), "");
        }
        std::fs::create_dir_all(root.join("sys/class/net/lo")).unwrap();
        std::fs::create_dir_all(root.join("sys/class/net/eth0")).unwrap();
        std::os::unix::fs::symlink(
            root.join("sys/devices/pci0"),
            root.join("sys/class/net/eth0/device"),
        )
        .unwrap();

        assert_eq!(physical_nics_at(&root.join("sys")).unwrap(), ["eth0"]);
        assert_eq!(
            rx_interrupts_at(&root.join("sys"), &root.join("proc")).unwrap(),
            [("eth0".to_owned(), vec![24, 25, 27])],
            "every receive queue, the spread one included, and not the config vector"
        );
        let layout = rx_layout_at(&root.join("sys"), &root.join("proc")).unwrap();
        let mut cpus: Vec<_> = layout.queues.iter().map(|queue| queue.cpu).collect();
        cpus.sort_unstable();
        assert_eq!(
            cpus,
            [0, 1],
            "the config vector and the spread queue are left out"
        );
        assert_eq!(layout.unserved(&[0, 1, 2]), [2]);

        let placement = Placement {
            cpus: vec![0, 1],
            layout: layout.clone(),
        };
        assert!(placement.steers());
        let placement = Placement {
            cpus: vec![0, 1, 2],
            layout,
        };
        assert!(!placement.steers(), "CPU 2 takes no receive queue");
    }
}
