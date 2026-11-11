//! The kernel half of steering: CPU pinning, `SO_INCOMING_CPU`, the reuseport program and the
//! NIC receive-queue layout.

use std::{
    collections::HashMap,
    io, mem,
    net::TcpListener,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use tracing::info;

use super::{Pins, Steering, SteeringError};
use crate::runtime::native::ListenAddr;

/// `SKF_AD_OFF + SKF_AD_CPU`: the ancillary load of the CPU the filter runs on, which for a
/// reuseport group is the CPU that took the SYN.
const LOAD_CPU: u32 = (-0x1000_i32 + 36) as u32;

/// Pin the calling thread to `cpu`.
pub(in crate::runtime::native) fn pin_current_thread(cpu: usize) -> io::Result<()> {
    // SAFETY: an all-zero `cpu_set_t` is the empty set, and `cpu` is below `CPU_SETSIZE`
    // because the mask it came from held it.
    let result = unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &raw const set)
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The CPUs this process may run on, in ascending order.
fn affinity_mask() -> io::Result<Vec<usize>> {
    // SAFETY: `set` is a valid, writable `cpu_set_t` of the size passed.
    let set = unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        if libc::sched_getaffinity(0, mem::size_of::<libc::cpu_set_t>(), &raw mut set) != 0 {
            return Err(io::Error::last_os_error());
        }
        set
    };
    // SAFETY: `cpu` is below `CPU_SETSIZE`.
    Ok((0..libc::CPU_SETSIZE as usize)
        .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
        .collect())
}

impl Pins {
    /// Thread `i` takes the `i`-th CPU of the process's affinity mask.
    ///
    /// # Errors
    ///
    /// The mask cannot be read or holds fewer than `threads` CPUs.
    pub(in crate::runtime::native) fn from_affinity(threads: usize) -> io::Result<Self> {
        let mut cpus = affinity_mask()?;
        if cpus.len() < threads {
            return Err(io::Error::other(format!(
                "{threads} serving threads but the process may run on {} CPUs",
                cpus.len()
            )));
        }
        cpus.truncate(threads);
        Ok(Self { cpus })
    }

    /// The CPU thread `index` serves, when steering.
    pub(in crate::runtime::native) fn cpu(&self, index: usize) -> Option<usize> {
        self.cpus.get(index).copied()
    }

    /// Whether any thread is pinned.
    pub(in crate::runtime::native) const fn is_steering(&self) -> bool {
        !self.cpus.is_empty()
    }

    /// Pin the calling thread, which is serving thread `index`, to its CPU.
    pub(in crate::runtime::native) fn pin(&self, index: usize) -> Result<(), SteeringError> {
        match self.cpu(index) {
            Some(cpu) => pin_current_thread(cpu).map_err(|error| SteeringError::Pin { cpu, error }),
            None => Ok(()),
        }
    }

    /// Attach the program that sends a connection to the listener of the CPU it arrived on.
    ///
    /// `listeners` are the group's members in the order they joined it, which is the index the
    /// program answers with; each must already carry its `SO_INCOMING_CPU`.
    ///
    /// # Errors
    ///
    /// The kernel refuses the program.
    pub(in crate::runtime::native) fn attach(&self, listeners: &[TcpListener]) -> io::Result<()> {
        let Some(first) = listeners.first().filter(|_| self.is_steering()) else {
            return Ok(());
        };
        let mut program = reuseport_program(&self.cpus);
        let filter = libc::sock_fprog {
            len: u16::try_from(program.len()).map_err(io::Error::other)?,
            filter: program.as_mut_ptr(),
        };
        // SAFETY: `filter` points at `len` instructions that outlive the call, and the kernel
        // copies them.
        let result = unsafe {
            libc::setsockopt(
                first.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ATTACH_REUSEPORT_CBPF,
                (&raw const filter).cast(),
                mem::size_of::<libc::sock_fprog>() as libc::socklen_t,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// The classic BPF program: load the CPU, and for each served CPU in turn return its listener's
/// index. A CPU no listener serves returns an index past the group, which makes the kernel fall
/// back to its hash.
fn reuseport_program(cpus: &[usize]) -> Vec<libc::sock_filter> {
    const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

    let insn = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let mut program = Vec::with_capacity(2 + 2 * cpus.len());
    program.push(insn(LD_W_ABS, 0, 0, LOAD_CPU));
    for (index, &cpu) in cpus.iter().enumerate() {
        program.push(insn(JEQ_K, 0, 1, cpu as u32));
        program.push(insn(RET_K, 0, 0, index as u32));
    }
    program.push(insn(RET_K, 0, 0, u32::MAX));
    program
}

/// Report to the kernel that `listener` serves `cpu`.
///
/// # Errors
///
/// The kernel refuses the option.
pub(in crate::runtime::native) fn set_incoming_cpu(
    listener: &socket2::Socket,
    cpu: usize,
) -> io::Result<()> {
    listener.set_cpu_affinity(cpu)
}

pub(super) fn decide(
    setting: Steering,
    threads: usize,
    addr: &ListenAddr,
) -> Result<Pins, SteeringError> {
    if setting == Steering::Off {
        return Ok(Pins::default());
    }
    if !matches!(addr, ListenAddr::Tcp(_)) {
        return match setting {
            Steering::On => Err(SteeringError::NotTcp),
            _ => {
                info!("Connection steering is off: the listener is a unix socket");
                Ok(Pins::default())
            }
        };
    }
    let pins = Pins::from_affinity(threads).map_err(SteeringError::Affinity)?;
    let unserved = rx_layout().map(|layout| layout.unserved(&pins.cpus));
    match (setting, unserved) {
        (_, Ok(unserved)) if unserved.is_empty() => {
            info!(
                cpus = ?pins.cpus,
                "Connection steering is on: every serving CPU takes a NIC receive queue"
            );
            Ok(pins)
        }
        (Steering::On, Ok(unserved)) => Err(SteeringError::Unserved(unserved)),
        (Steering::On, Err(error)) => Err(SteeringError::Layout(error)),
        (_, Ok(unserved)) => {
            info!(
                cpus = ?unserved,
                "Connection steering is off: these serving CPUs take no NIC receive queue's \
                 interrupt (force it with --steering on)"
            );
            Ok(Pins::default())
        }
        (_, Err(error)) => {
            info!("Connection steering is off: the NIC queue layout cannot be read: {error}");
            Ok(Pins::default())
        }
    }
}

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
    let names = interrupt_names(&proc.join("interrupts"))?;
    let mut nics = std::fs::read_dir(sys.join("class/net"))?.collect::<io::Result<Vec<_>>>()?;
    nics.sort_by_key(std::fs::DirEntry::file_name);
    let mut queues = Vec::new();
    for nic in nics {
        let device = nic.path().join("device");
        // A virtual interface (loopback, bridge, veth) has no device behind it.
        if !device.exists() {
            continue;
        }
        let nic = nic.file_name().to_string_lossy().into_owned();
        for irq in device_irqs(&device)? {
            let Some(name) = names.get(&irq) else {
                continue;
            };
            if !is_receive_name(name) {
                continue;
            }
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
        .or_else(|_| std::fs::read_to_string(dir.join("smp_affinity_list")))
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
            Some((first, last)) => cpus.extend(first.parse::<usize>().ok()?..=last.parse().ok()?),
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

    #[test]
    fn the_program_maps_each_served_cpu_to_its_listener() {
        let program = reuseport_program(&[2, 5]);
        assert_eq!(program.len(), 6);
        assert_eq!((program[1].k, program[2].k), (2, 0));
        assert_eq!((program[3].k, program[4].k), (5, 1));
        assert_eq!(program[5].k, u32::MAX);
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

        let layout = rx_layout_at(&root.join("sys"), &root.join("proc")).unwrap();
        let cpus: Vec<_> = layout.queues.iter().map(|queue| queue.cpu).collect();
        assert_eq!(
            cpus,
            [0, 1],
            "the config vector and the spread queue are left out"
        );
        assert_eq!(layout.unserved(&[0, 1, 2]), [2]);
    }
}
